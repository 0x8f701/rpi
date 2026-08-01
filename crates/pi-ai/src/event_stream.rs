use crate::{AssistantMessage, StopReason, ToolCall};
use serde::{Deserialize, Serialize};
use std::{collections::VecDeque, sync::Arc};
use tokio::sync::{Mutex, Notify};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AssistantMessageEvent {
    Start {
        partial: AssistantMessage,
    },
    TextStart {
        content_index: usize,
        partial: AssistantMessage,
    },
    TextDelta {
        content_index: usize,
        delta: String,
        partial: AssistantMessage,
    },
    TextEnd {
        content_index: usize,
        content: String,
        partial: AssistantMessage,
    },
    ThinkingStart {
        content_index: usize,
        partial: AssistantMessage,
    },
    ThinkingDelta {
        content_index: usize,
        delta: String,
        partial: AssistantMessage,
    },
    ThinkingEnd {
        content_index: usize,
        content: String,
        partial: AssistantMessage,
    },
    #[serde(rename = "toolcall_start")]
    ToolCallStart {
        content_index: usize,
        partial: AssistantMessage,
    },
    #[serde(rename = "toolcall_delta")]
    ToolCallDelta {
        content_index: usize,
        delta: String,
        partial: AssistantMessage,
    },
    #[serde(rename = "toolcall_end")]
    ToolCallEnd {
        content_index: usize,
        tool_call: ToolCall,
        partial: AssistantMessage,
    },
    Done {
        reason: StopReason,
        message: AssistantMessage,
    },
    Error {
        reason: StopReason,
        error: AssistantMessage,
    },
}
impl AssistantMessageEvent {
    pub fn partial(&self) -> Option<&AssistantMessage> {
        match self {
            Self::Start { partial }
            | Self::TextStart { partial, .. }
            | Self::TextDelta { partial, .. }
            | Self::TextEnd { partial, .. }
            | Self::ThinkingStart { partial, .. }
            | Self::ThinkingDelta { partial, .. }
            | Self::ThinkingEnd { partial, .. }
            | Self::ToolCallStart { partial, .. }
            | Self::ToolCallDelta { partial, .. }
            | Self::ToolCallEnd { partial, .. } => Some(partial),
            Self::Done { .. } | Self::Error { .. } => None,
        }
    }
    pub fn terminal_message(&self) -> Option<&AssistantMessage> {
        match self {
            Self::Done { message, .. } => Some(message),
            Self::Error { error, .. } => Some(error),
            _ => None,
        }
    }
}
struct State<T, R> {
    queue: VecDeque<T>,
    done: bool,
    result: Option<R>,
}
#[derive(Clone)]
pub struct EventStream<T, R> {
    state: Arc<Mutex<State<T, R>>>,
    notify: Arc<Notify>,
}
impl<T, R> Default for EventStream<T, R> {
    fn default() -> Self {
        Self::new()
    }
}
impl<T, R> EventStream<T, R> {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                queue: VecDeque::new(),
                done: false,
                result: None,
            })),
            notify: Arc::new(Notify::new()),
        }
    }
    pub async fn push(&self, event: T) {
        let mut state = self.state.lock().await;
        if !state.done {
            state.queue.push_back(event);
            self.notify.notify_waiters();
        }
    }
    pub async fn end(&self, result: Option<R>) {
        let mut state = self.state.lock().await;
        if state.result.is_none() {
            state.result = result;
        }
        state.done = true;
        self.notify.notify_waiters();
    }
    pub async fn next(&self) -> Option<T> {
        loop {
            let notified = self.notify.notified();
            {
                let mut state = self.state.lock().await;
                if let Some(event) = state.queue.pop_front() {
                    return Some(event);
                }
                if state.done {
                    return None;
                }
            }
            notified.await;
        }
    }
}
impl<T, R: Clone> EventStream<T, R> {
    pub async fn result(&self) -> Option<R> {
        loop {
            let notified = self.notify.notified();
            {
                let state = self.state.lock().await;
                if let Some(result) = &state.result {
                    return Some(result.clone());
                }
                if state.done {
                    return None;
                }
            }
            notified.await;
        }
    }
}
pub type AssistantMessageEventStream = EventStream<AssistantMessageEvent, AssistantMessage>;
pub fn new_assistant_message_event_stream() -> AssistantMessageEventStream {
    EventStream::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn preserves_order_and_terminal_result() {
        let stream: EventStream<i32, String> = EventStream::new();
        stream.push(1).await;
        stream.push(2).await;
        stream.end(Some("done".into())).await;
        assert_eq!(stream.next().await, Some(1));
        assert_eq!(stream.next().await, Some(2));
        assert_eq!(stream.next().await, None);
        assert_eq!(stream.result().await.as_deref(), Some("done"));
    }
    #[tokio::test]
    async fn waiter_cannot_miss_completion_notification() {
        for _ in 0..500 {
            let stream: EventStream<(), usize> = EventStream::new();
            let waiter = {
                let stream = stream.clone();
                tokio::spawn(async move { stream.result().await })
            };
            stream.end(Some(7)).await;
            assert_eq!(waiter.await.expect("join waiter"), Some(7));
        }
    }
}
