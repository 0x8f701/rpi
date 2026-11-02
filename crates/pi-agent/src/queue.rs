use std::collections::VecDeque;

use pi_ai::{ContentBlock, Message, UserMessage, now_millis};

use crate::{AgentMessage, QueueMode};

#[derive(Clone, Debug)]
pub(crate) struct PendingQueue {
    mode: QueueMode,
    messages: VecDeque<AgentMessage>,
}

impl PendingQueue {
    pub(crate) fn new(mode: QueueMode) -> Self {
        Self {
            mode,
            messages: VecDeque::new(),
        }
    }

    pub(crate) fn enqueue(&mut self, message: AgentMessage) {
        self.messages.push_back(message);
    }

    /// Re-enqueue drained messages at the HEAD of the queue, preserving their
    /// relative FIFO order. Used by the transactional-drain paths: a message
    /// that left the queue without being durably recorded must return ahead of
    /// everything enqueued since, in the order it originally occupied.
    pub(crate) fn enqueue_front(&mut self, messages: Vec<AgentMessage>) {
        for message in messages.into_iter().rev() {
            self.messages.push_front(message);
        }
    }

    pub(crate) const fn mode(&self) -> QueueMode {
        self.mode
    }

    pub(crate) fn set_mode(&mut self, mode: QueueMode) {
        self.mode = mode;
    }

    pub(crate) fn len(&self) -> usize {
        self.messages.len()
    }

    pub(crate) fn has_items(&self) -> bool {
        !self.messages.is_empty()
    }

    pub(crate) fn clear(&mut self) {
        self.messages.clear();
    }
    pub(crate) fn snapshot(&self) -> Vec<AgentMessage> {
        self.messages.iter().cloned().collect()
    }

    pub(crate) fn drain_all(&mut self) -> Vec<AgentMessage> {
        self.messages.drain(..).collect()
    }

    pub(crate) fn remove_at(&mut self, index: usize) -> Option<AgentMessage> {
        self.messages.remove(index)
    }

    pub(crate) fn drain(&mut self) -> Vec<AgentMessage> {
        match self.mode {
            QueueMode::All => self.messages.drain(..).collect(),
            QueueMode::OneAtATime => self.messages.pop_front().into_iter().collect(),
        }
    }
}

pub fn queued_user_previews(messages: &[AgentMessage]) -> Vec<String> {
    messages.iter().filter_map(queued_user_preview).collect()
}

pub fn queued_user_preview(message: &AgentMessage) -> Option<String> {
    let Message::User(user) = message else {
        return None;
    };
    let mut texts = Vec::new();
    let mut has_image = false;
    for block in &user.content {
        match block {
            ContentBlock::Text { text, .. } if !text.is_empty() => texts.push(text.as_str()),
            ContentBlock::Image { .. } => has_image = true,
            _ => {}
        }
    }
    let joined = texts.join("\n\n");
    if !joined.trim().is_empty() {
        Some(joined)
    } else if has_image {
        Some("(image)".to_owned())
    } else {
        None
    }
}

pub fn split_queued_user_message(message: &AgentMessage) -> (String, Vec<ContentBlock>) {
    let Message::User(user) = message else {
        return (String::new(), Vec::new());
    };
    let mut texts = Vec::new();
    let mut images = Vec::new();
    for block in &user.content {
        match block {
            ContentBlock::Text { text, .. } if !text.is_empty() => texts.push(text.clone()),
            ContentBlock::Image { .. } => images.push(block.clone()),
            _ => {}
        }
    }
    (texts.join("\n\n"), images)
}

pub fn combine_queued_user_messages(messages: Vec<AgentMessage>) -> Option<AgentMessage> {
    let mut content: Vec<ContentBlock> = Vec::new();
    let mut saw_user = false;
    for message in messages {
        let Message::User(user) = message else {
            continue;
        };
        saw_user = true;
        for block in user.content {
            match (content.last_mut(), block) {
                (
                    Some(ContentBlock::Text { text, .. }),
                    ContentBlock::Text {
                        text: next,
                        text_signature: None,
                    },
                ) if !next.is_empty() => {
                    if !text.is_empty() {
                        text.push_str("\n\n");
                    }
                    text.push_str(&next);
                }
                (_, block) => content.push(block),
            }
        }
    }
    if !saw_user || content.is_empty() {
        return None;
    }
    Some(Message::User(UserMessage {
        content,
        timestamp: now_millis(),
    }))
}

#[cfg(test)]
mod tests {
    use super::{
        PendingQueue, combine_queued_user_messages, queued_user_previews, split_queued_user_message,
    };
    use crate::QueueMode;
    use pi_ai::{ContentBlock, Message};

    #[test]
    fn remove_at_drops_only_the_indexed_message() {
        let mut queue = PendingQueue::new(QueueMode::All);
        queue.enqueue(Message::user_text("one", 1));
        queue.enqueue(Message::user_text("two", 2));
        queue.enqueue(Message::user_text("three", 3));
        assert_eq!(
            queue.remove_at(1),
            Some(Message::user_text("two", 2))
        );
        assert_eq!(
            queue.snapshot(),
            vec![
                Message::user_text("one", 1),
                Message::user_text("three", 3)
            ]
        );
        assert_eq!(queue.remove_at(9), None);
    }

    #[test]
    fn combine_joins_texts_and_keeps_images_in_order() {
        let first = Message::User(pi_ai::UserMessage {
            content: vec![
                ContentBlock::text("first"),
                ContentBlock::Image {
                    data: "aaa".to_owned(),
                    mime_type: "image/png".to_owned(),
                },
            ],
            timestamp: 1,
        });
        let second = Message::user_text("second", 2);
        let combined = combine_queued_user_messages(vec![first, second]).expect("combined");
        let (text, images) = split_queued_user_message(&combined);
        assert_eq!(text, "first\n\nsecond");
        assert_eq!(images.len(), 1);
        assert_eq!(
            queued_user_previews(&[combined]),
            vec!["first\n\nsecond".to_owned()]
        );
    }

    #[test]
    fn combine_rejects_empty_non_user_queue() {
        assert!(combine_queued_user_messages(Vec::new()).is_none());
    }
}
