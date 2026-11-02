//! Regression tests for the steering-loss fixes (F1): a steering message must
//! leave the pending queue only when it is durably recorded, or return to the
//! queue head — in its original FIFO position — when delivery fails.
//!
//! These tests exercise the public `pi_agent` API only and own no source
//! files; they mirror the deleted repro recipes from the steering-loss audit
//! (scripted `StreamFn`, gated `AgentTool`, injected hooks).

use std::collections::VecDeque;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use parking_lot::Mutex;
use pi_agent::{
    Agent, AgentOptions, AgentState, AgentTool, AgentToolResult, BeforeAgentStartFn,
    BeforeAgentStartResult, BoxFuture, StreamFn, TransformMessageFn,
};
use pi_ai::{
    AssistantMessage, AssistantMessageEvent, ContentBlock, Context, Message, Model, Schema,
    SimpleStreamOptions, StopReason, ToolCall, new_assistant_message_event_stream,
};
use serde_json::json;

fn model() -> Model {
    Model {
        id: "scripted".into(),
        name: "Scripted".into(),
        api: "scripted".into(),
        provider: "scripted".into(),
        ..Model::default()
    }
}

fn assistant(content: Vec<ContentBlock>, stop_reason: StopReason) -> AssistantMessage {
    AssistantMessage {
        content,
        api: "scripted".into(),
        provider: "scripted".into(),
        model: "scripted".into(),
        response_model: None,
        response_id: None,
        diagnostics: vec![],
        usage: Default::default(),
        stop_reason,
        error_message: None,
        raw_stop_reason: None,
        timestamp: pi_ai::now_millis(),
    }
}

fn call(id: &str, name: &str) -> ContentBlock {
    ContentBlock::ToolCall(ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: json!({}),
        thought_signature: None,
    })
}

fn user_text(message: &Message) -> String {
    match message {
        Message::User(user) => user
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

fn scripted(messages: Vec<AssistantMessage>) -> StreamFn {
    let messages = Arc::new(Mutex::new(VecDeque::from(messages)));
    Arc::new(
        move |_model: Model, _context: Context, _options: SimpleStreamOptions| {
            let messages = messages.clone();
            Box::pin(async move {
                let message = messages.lock().pop_front().unwrap_or_else(|| {
                    assistant(vec![ContentBlock::text("done")], StopReason::Stop)
                });
                let stream = new_assistant_message_event_stream();
                let producer = stream.clone();
                tokio::spawn(async move {
                    producer
                        .push(AssistantMessageEvent::Start {
                            partial: AssistantMessage::pending(&model()),
                        })
                        .await;
                    let terminal =
                        if matches!(message.stop_reason, StopReason::Error | StopReason::Aborted) {
                            AssistantMessageEvent::Error {
                                reason: message.stop_reason,
                                error: message.clone(),
                            }
                        } else {
                            AssistantMessageEvent::Done {
                                reason: message.stop_reason,
                                message: message.clone(),
                            }
                        };
                    producer.push(terminal).await;
                    producer.end(Some(message)).await;
                });
                stream
            })
        },
    )
}

fn options(stream_fn: StreamFn, tools: Vec<AgentTool>) -> AgentOptions {
    AgentOptions {
        initial_state: AgentState {
            model: model(),
            tools,
            ..AgentState::default()
        },
        stream_fn,
        ..AgentOptions::default()
    }
}

/// A tool that parks until released, opening a deterministic mid-turn window:
/// the test steers while the tool is executing, before the TurnEnd boundary
/// drain can observe the queue.
fn gate_tool(
    started: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
) -> AgentTool {
    AgentTool::new("gate", "gate", Schema::default(), move |_context| {
        let started = started.clone();
        let release = release.clone();
        async move {
            started.notify_one();
            release.notified().await;
            Ok(AgentToolResult::text("gated"))
        }
    })
}

fn steer_texts(messages: &[Message]) -> Vec<String> {
    messages.iter().map(user_text).collect()
}

/// D1: `continue_run` drains the steering queue before the (fallible)
/// `prepare_agent_start` step. A failing `before_agent_start` hook must
/// re-enqueue the drained messages at the queue HEAD in their original FIFO
/// order — never silently consume acknowledged steering.
#[tokio::test]
async fn failing_before_agent_start_preserves_continue_run_drained_steering() {
    let agent = Agent::new(options(
        scripted(vec![
            assistant(vec![ContentBlock::text("first")], StopReason::Stop),
            assistant(vec![ContentBlock::text("second")], StopReason::Stop),
        ]),
        vec![],
    ));
    agent.prompt("go").await.expect("initial run");

    agent.steer(Message::user_text("fix it", 1)).await;
    agent.steer(Message::user_text("also this", 2)).await;
    let hook: BeforeAgentStartFn =
        Arc::new(|_| -> BoxFuture<Result<BeforeAgentStartResult>> {
            Box::pin(async { Err(anyhow!("injected before_agent_start failure")) })
        });
    agent.set_before_agent_start(Some(hook));

    let error = agent
        .continue_run()
        .await
        .expect_err("prepare step must fail");
    assert!(
        error.to_string().contains("injected before_agent_start failure"),
        "{error:#}"
    );

    // The drained steering survived: both messages, in original FIFO order.
    assert_eq!(agent.pending_message_count().await, 2);
    let (steering, follow_up) = agent.queued_messages().await;
    assert_eq!(steer_texts(&steering), vec!["fix it", "also this"]);
    assert!(follow_up.is_empty());

    // Recovery: clearing the hook lets the retry deliver the head message.
    // The failed prepare released its claim through the guard's asynchronous
    // drop cleanup, so poll briefly until the agent is claimable again.
    agent.set_before_agent_start(None);
    let mut retry = Err(anyhow!("not retried"));
    for attempt in 0..200 {
        retry = agent.continue_run().await;
        if retry.is_ok() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let _ = attempt;
    }
    retry.expect("retry succeeds");
    let state = agent.state().await;
    assert!(
        state.messages.iter().any(|message| user_text(message) == "fix it"),
        "steering must be delivered after the retry: {state:?}"
    );
    // FIFO preserved: `fix it` was delivered first (the continue's initial
    // message), then `also this` at the run-loop turn boundary.
    let texts: Vec<String> = state
        .messages
        .iter()
        .filter(|message| matches!(message, Message::User(_)))
        .map(|message| user_text(message))
        .collect();
    assert_eq!(texts, vec!["go", "fix it", "also this"]);
    assert_eq!(agent.pending_message_count().await, 0);
}

/// D2: the run loop drains pending steering at the turn boundary and delivers
/// it at the start of the next iteration. A failing `transform_message` on the
/// boundary-drained message must re-enqueue it — the message must not leave
/// the queue without being recorded — and the agent must not wedge.
#[tokio::test]
async fn failing_transform_on_boundary_drained_steering_re_enqueues() {
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let agent = Agent::new(options(
        scripted(vec![
            assistant(vec![call("g", "gate")], StopReason::ToolUse),
            assistant(vec![ContentBlock::text("two")], StopReason::Stop),
            assistant(vec![ContentBlock::text("three")], StopReason::Stop),
        ]),
        vec![gate_tool(started.clone(), release.clone())],
    ));
    let target = "fix it".to_owned();
    let hook: TransformMessageFn = Arc::new(move |message, _signal| {
        let hits = matches!(&message, Message::User(user)
            if user.content.iter().any(|block| matches!(block, ContentBlock::Text { text, .. } if text == &target)));
        Box::pin(async move {
            if hits {
                Err(anyhow!("injected transform failure"))
            } else {
                Ok(message)
            }
        })
    });
    agent.set_transform_message(Some(hook));

    let run = tokio::spawn({
        let agent = agent.clone();
        async move { agent.prompt("go").await }
    });
    started.notified().await;
    agent.steer(Message::user_text("fix it", 1)).await;
    release.notify_waiters();

    let error = run
        .await
        .expect("run task joins")
        .expect_err("the injected transform failure must end the run");
    assert!(error.to_string().contains("injected transform failure"), "{error:#}");

    // The boundary-drained steering survived the failing emit: queued again at
    // the head, never delivered into the agent state.
    assert_eq!(agent.pending_message_count().await, 1);
    let (steering, follow_up) = agent.queued_messages().await;
    assert_eq!(steer_texts(&steering), vec!["fix it"]);
    assert!(follow_up.is_empty());
    let state = agent.state().await;
    assert!(
        state.messages.iter().all(|message| user_text(message) != "fix it"),
        "the failed steering must not be in the transcript: {state:?}"
    );

    // No wedge: clearing the hook lets a continuation deliver the message.
    agent.set_transform_message(None);
    agent.continue_run().await.expect("continuation succeeds");
    let state = agent.state().await;
    assert!(
        state.messages.iter().any(|message| user_text(message) == "fix it"),
        "re-enqueued steering must be delivered by the continuation: {state:?}"
    );
    assert_eq!(agent.pending_message_count().await, 0);
}