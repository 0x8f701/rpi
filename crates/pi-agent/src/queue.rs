use std::collections::VecDeque;

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

    pub(crate) fn drain(&mut self) -> Vec<AgentMessage> {
        match self.mode {
            QueueMode::All => self.messages.drain(..).collect(),
            QueueMode::OneAtATime => self.messages.pop_front().into_iter().collect(),
        }
    }
}
