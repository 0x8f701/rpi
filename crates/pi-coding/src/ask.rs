//! Interactive `ask` tool runtime: the single-pending user-question round trip.
//!
//! The `ask` tool lets the model ask the user a question mid-task and receive
//! the typed answer as the tool result. This module owns the pending-question
//! slot (at most one at a time), publishes [`SessionEvent::AskUser`] so the
//! frontend can render the prompt, and resolves the awaiting tool call when
//! the frontend delivers the answer via [`AskRuntime::answer`] /
//! [`AskRuntime::cancel`].
//!
//! Non-interactive frontends (print/JSON/RPC/REPL) never arm the interactive
//! flag, so the tool rejects up front with an actionable error instead of
//! hanging for the timeout.

use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::Duration;

use anyhow::{Result, anyhow};
use parking_lot::{Mutex, RwLock};
use pi_agent::{AbortSignal, AgentToolResult};
use tokio::sync::{broadcast, oneshot};

use crate::SessionEvent;

/// Default bound on how long a pending `ask` waits for the user's answer.
pub const DEFAULT_ASK_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug)]
enum AskReply {
    Answer(String),
    Cancelled,
}

struct PendingAsk {
    id: String,
    prompt: String,
    reply: oneshot::Sender<AskReply>,
}

struct AskInner {
    pending: Mutex<Option<PendingAsk>>,
    interactive: AtomicBool,
    timeout: RwLock<Duration>,
    events: broadcast::Sender<SessionEvent>,
    next_id: AtomicU64,
}

/// Cloneable handle to the session's ask slot. All methods are cheap and
/// thread-safe; `request` is the only async entry point (it awaits the user).
#[derive(Clone)]
pub(crate) struct AskRuntime {
    inner: Arc<AskInner>,
}

impl AskRuntime {
    pub(crate) fn new(events: broadcast::Sender<SessionEvent>) -> Self {
        Self {
            inner: Arc::new(AskInner {
                pending: Mutex::new(None),
                interactive: AtomicBool::new(false),
                timeout: RwLock::new(DEFAULT_ASK_TIMEOUT),
                events,
                next_id: AtomicU64::new(1),
            }),
        }
    }

    /// Enable or disable the interactive round trip. Only interactive
    /// frontends (the TUI) arm this; every other mode rejects `ask` up front.
    pub(crate) fn set_interactive(&self, interactive: bool) {
        self.inner.interactive.store(interactive, Ordering::Release);
    }

    /// Override the answer-wait bound. Tests use short values; the TUI keeps
    /// the default.
    pub(crate) fn set_timeout(&self, timeout: Duration) {
        *self.inner.timeout.write() = timeout;
    }

    /// The currently pending ask as `(id, prompt)`, if any.
    pub(crate) fn pending(&self) -> Option<(String, String)> {
        self.inner
            .pending
            .lock()
            .as_ref()
            .map(|pending| (pending.id.clone(), pending.prompt.clone()))
    }

    /// Register a question and await the user's answer.
    ///
    /// Errors (rather than blocking) when the session is not interactive, when
    /// another question is already pending, when the user cancels, or when the
    /// answer-wait bound elapses. The run's `abort` signal also cancels the
    /// wait so a turn abort cannot strand the pending slot.
    pub(crate) async fn request(
        &self,
        question: String,
        abort: AbortSignal,
    ) -> Result<AgentToolResult> {
        if !self.inner.interactive.load(Ordering::Acquire) {
            return Err(anyhow!(
                "ask requires an interactive session; this mode cannot prompt the user"
            ));
        }
        let id = format!("ask-{}", self.inner.next_id.fetch_add(1, Ordering::Relaxed));
        let (reply_tx, reply_rx) = oneshot::channel();
        {
            let mut pending = self.inner.pending.lock();
            if pending.is_some() {
                return Err(anyhow!(
                    "another question is already pending; answer or cancel it first"
                ));
            }
            *pending = Some(PendingAsk {
                id: id.clone(),
                prompt: question.clone(),
                reply: reply_tx,
            });
        }
        let _ = self
            .inner
            .events
            .send(SessionEvent::AskUser { id: id.clone(), prompt: question });
        let timeout = *self.inner.timeout.read();
        let outcome = tokio::select! {
            _ = abort.cancelled() => Some(AskReply::Cancelled),
            reply = reply_rx => reply.ok(),
            _ = tokio::time::sleep(timeout) => None,
        };
        // Deregister our own slot on the timeout/abort paths (an answer or
        // cancel already consumed it). Dropping the sender makes any late
        // `answer`/`cancel` fail harmlessly instead of stranding the slot.
        {
            let mut pending = self.inner.pending.lock();
            if pending.as_ref().is_some_and(|pending| pending.id == id) {
                pending.take();
            }
        }
        // Every resolution path (answer, cancel, timeout, abort) publishes the
        // resolution so the frontend clears the rendered question.
        let _ = self.inner.events.send(SessionEvent::AskUserResolved { id });
        match outcome {
            Some(AskReply::Answer(answer)) => Ok(AgentToolResult::text(answer)),
            Some(AskReply::Cancelled) => Err(anyhow!("ask cancelled")),
            None => Err(anyhow!("timed out waiting for user")),
        }
    }

    /// Deliver the user's answer to the pending ask. Errors when there is no
    /// pending ask, when `id` does not match, or when it already resolved.
    pub(crate) fn answer(&self, id: &str, answer: String) -> Result<()> {
        let mut pending = self.inner.pending.lock();
        let Some(slot) = pending.take() else {
            return Err(anyhow!("no question is currently pending"));
        };
        if slot.id != id {
            *pending = Some(slot);
            return Err(anyhow!("pending question id mismatch"));
        }
        slot.reply
            .send(AskReply::Answer(answer))
            .map_err(|_| anyhow!("question already resolved"))
    }

    /// Cancel the pending ask (Esc / shutdown). Errors like [`AskRuntime::answer`].
    pub(crate) fn cancel(&self, id: &str) -> Result<()> {
        let mut pending = self.inner.pending.lock();
        let Some(slot) = pending.take() else {
            return Err(anyhow!("no question is currently pending"));
        };
        if slot.id != id {
            *pending = Some(slot);
            return Err(anyhow!("pending question id mismatch"));
        }
        slot.reply
            .send(AskReply::Cancelled)
            .map_err(|_| anyhow!("question already resolved"))
    }

    /// Cancel whatever is pending, regardless of id (TUI shutdown). Returns
    /// whether a pending ask was cancelled.
    pub(crate) fn cancel_pending(&self) -> bool {
        let Some(slot) = self.inner.pending.lock().take() else {
            return false;
        };
        let _ = slot.reply.send(AskReply::Cancelled);
        true
    }
}

#[cfg(test)]
mod ask_runtime_tests {
    use std::time::Duration;

    use pi_agent::AbortController;
    use serde_json::json;

    use super::*;
    use crate::SessionEvent;

    fn runtime() -> (AskRuntime, broadcast::Receiver<SessionEvent>) {
        let (events, receiver) = broadcast::channel(16);
        (AskRuntime::new(events), receiver)
    }

    #[tokio::test]
    async fn request_publishes_ask_user_event() {
        let (runtime, mut receiver) = runtime();
        runtime.set_interactive(true);
        let (_, abort) = AbortController::new();
        let task = tokio::spawn({
            let runtime = runtime.clone();
            async move { runtime.request("proceed?".to_owned(), abort).await }
        });
        match receiver.recv().await.expect("ask event") {
            SessionEvent::AskUser { id, prompt } => {
                assert_eq!(prompt, "proceed?");
                assert!(id.starts_with("ask-"));
                runtime.answer(&id, "yes".to_owned()).expect("answer");
            }
            other => panic!("expected AskUser, got {other:?}"),
        }
        let result = task.await.expect("join").expect("request succeeds");
        assert_eq!(result.content, vec![pi_ai::ContentBlock::text("yes")]);
    }

    #[tokio::test]
    async fn non_interactive_session_rejects_up_front() {
        let (runtime, _receiver) = runtime();
        let (_, abort) = AbortController::new();
        let error = runtime
            .request("proceed?".to_owned(), abort)
            .await
            .expect_err("non-interactive must reject");
        assert!(error.to_string().contains("interactive"));
        assert!(runtime.pending().is_none());
    }

    #[tokio::test]
    async fn concurrent_ask_is_rejected_busy() {
        let (runtime, mut receiver) = runtime();
        runtime.set_interactive(true);
        let (_, abort) = AbortController::new();
        let first = tokio::spawn({
            let runtime = runtime.clone();
            async move { runtime.request("first?".to_owned(), abort).await }
        });
        let (_, second_abort) = AbortController::new();
        let second = tokio::spawn({
            let runtime = runtime.clone();
            async move { runtime.request("second?".to_owned(), second_abort).await }
        });
        match receiver.recv().await.expect("ask event") {
            SessionEvent::AskUser { id, .. } => {
                // Exactly one of the two calls won the slot; the loser failed
                // with the busy error and the winner still awaits the answer.
                // Answer before joining: on the single-thread test runtime the
                // winner only resolves once the answer is delivered.
                runtime.answer(&id, "yes".to_owned()).expect("answer");
                let first = first.await.expect("join");
                let second = second.await.expect("join");
                let (winner, loser) = match (first, second) {
                    (Ok(_), Err(error)) => (None, Some(error)),
                    (Err(error), Ok(_)) => (Some(error), None),
                    other => panic!("expected exactly one busy error, got {other:?}"),
                };
                let loser = loser.or(winner).expect("one busy error");
                assert!(loser.to_string().contains("already pending"));
            }
            other => panic!("expected AskUser, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unanswered_ask_times_out() {
        let (runtime, mut receiver) = runtime();
        runtime.set_interactive(true);
        runtime.set_timeout(Duration::from_millis(50));
        let (_, abort) = AbortController::new();
        let task = tokio::spawn({
            let runtime = runtime.clone();
            async move { runtime.request("proceed?".to_owned(), abort).await }
        });
        match receiver.recv().await.expect("ask event") {
            SessionEvent::AskUser { id, .. } => {
                assert_eq!(runtime.pending(), Some((id, "proceed?".to_owned())));
            }
            other => panic!("expected AskUser, got {other:?}"),
        }
        let error = task.await.expect("join").expect_err("timeout must reject");
        assert!(error.to_string().contains("timed out waiting for user"));
        assert!(runtime.pending().is_none(), "timeout must free the slot");
        // A late answer after timeout is rejected, not applied.
        assert!(runtime.answer("ask-1", "late".to_owned()).is_err());
    }

    #[tokio::test]
    async fn cancel_resolves_with_cancelled_error() {
        let (runtime, mut receiver) = runtime();
        runtime.set_interactive(true);
        let (_, abort) = AbortController::new();
        let task = tokio::spawn({
            let runtime = runtime.clone();
            async move { runtime.request("proceed?".to_owned(), abort).await }
        });
        let id = match receiver.recv().await.expect("ask event") {
            SessionEvent::AskUser { id, .. } => id,
            other => panic!("expected AskUser, got {other:?}"),
        };
        runtime.cancel(&id).expect("cancel");
        let error = task.await.expect("join").expect_err("cancel must reject");
        assert!(error.to_string().contains("cancelled"));
        assert!(runtime.pending().is_none());
    }

    #[tokio::test]
    async fn run_abort_cancels_the_wait_and_frees_the_slot() {
        let (runtime, mut receiver) = runtime();
        runtime.set_interactive(true);
        let (controller, abort) = AbortController::new();
        let task = tokio::spawn({
            let runtime = runtime.clone();
            async move { runtime.request("proceed?".to_owned(), abort).await }
        });
        match receiver.recv().await.expect("ask event") {
            SessionEvent::AskUser { id, .. } => {
                assert_eq!(runtime.pending(), Some((id, "proceed?".to_owned())));
            }
            other => panic!("expected AskUser, got {other:?}"),
        }
        controller.abort();
        let error = task.await.expect("join").expect_err("abort must reject");
        assert!(error.to_string().contains("cancelled"));
        assert!(runtime.pending().is_none(), "abort must free the slot");
    }

    #[tokio::test]
    async fn wrong_id_answer_is_rejected_and_keeps_the_ask_pending() {
        let (runtime, mut receiver) = runtime();
        runtime.set_interactive(true);
        let (_, abort) = AbortController::new();
        let task = tokio::spawn({
            let runtime = runtime.clone();
            async move { runtime.request("proceed?".to_owned(), abort).await }
        });
        let id = match receiver.recv().await.expect("ask event") {
            SessionEvent::AskUser { id, .. } => id,
            other => panic!("expected AskUser, got {other:?}"),
        };
        assert!(runtime.answer("wrong-id", "nope".to_owned()).is_err());
        assert_eq!(runtime.pending().as_ref().map(|(pending_id, _)| pending_id), Some(&id));
        runtime.answer(&id, "ok".to_owned()).expect("answer");
        let result = task.await.expect("join").expect("request succeeds");
        assert_eq!(result.content, vec![pi_ai::ContentBlock::text("ok")]);
    }

    #[test]
    fn json_serialization_shape_is_stable() {
        let event = SessionEvent::AskUser {
            id: "ask-1".to_owned(),
            prompt: "proceed?".to_owned(),
        };
        assert_eq!(
            serde_json::to_value(&event).expect("serialize"),
            json!({ "type": "ask_user", "id": "ask-1", "prompt": "proceed?" })
        );
        let resolved = SessionEvent::AskUserResolved { id: "ask-1".to_owned() };
        assert_eq!(
            serde_json::to_value(&resolved).expect("serialize"),
            json!({ "type": "ask_user_resolved", "id": "ask-1" })
        );
    }
}
