use crate::{
    Agent, AgentEvent, AgentEventType, AgentLoopTurnUpdate, AgentOptions, AgentState, AgentTool,
    AgentToolResult, BeforeToolCallResult, QueueMode, StreamFn, ToolExecutionMode,
};
use anyhow::anyhow;
use parking_lot::Mutex;
use pi_ai::{
    AssistantMessage, AssistantMessageEvent, ContentBlock, Context, Message, Model, Schema,
    SimpleStreamOptions, StopReason, ThinkingBudgets, ToolCall, ToolResultMessage, Transport,
    new_assistant_message_event_stream,
};
use serde_json::json;
use std::{
    collections::VecDeque,
    sync::Arc,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    time::Duration,
};
use tokio::sync::{Barrier, Notify};

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
fn result_at(state: &AgentState, index: usize) -> &ToolResultMessage {
    match &state.messages[index] {
        Message::ToolResult(result) => result,
        message => panic!("not result: {message:?}"),
    }
}
fn assistant_at(state: &AgentState, index: usize) -> &AssistantMessage {
    match &state.messages[index] {
        Message::Assistant(message) => message,
        message => panic!("not assistant: {message:?}"),
    }
}
fn tool_call_ids(message: &AssistantMessage) -> Vec<String> {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolCall(call) => Some(call.id.clone()),
            _ => None,
        })
        .collect()
}
fn is_synthesized_tool_call_id(id: &str) -> bool {
    id.len() == 9
        && id.starts_with('p')
        && id.chars().all(|ch| ch.is_ascii_alphanumeric())
}


#[tokio::test]
async fn no_tool_turn_has_complete_lifecycle() {
    let agent = Agent::new(options(
        scripted(vec![assistant(
            vec![ContentBlock::text("hello")],
            StopReason::Stop,
        )]),
        vec![],
    ));
    let events = Arc::new(Mutex::new(vec![]));
    let recorded = events.clone();
    let _sub = agent
        .subscribe_simple(move |event| {
            let recorded = recorded.clone();
            async move {
                recorded.lock().push(event.event_type());
                Ok(())
            }
        })
        .await;
    agent.prompt("hi").await.unwrap();
    assert_eq!(
        *events.lock(),
        vec![
            AgentEventType::AgentStart,
            AgentEventType::TurnStart,
            AgentEventType::MessageStart,
            AgentEventType::MessageEnd,
            AgentEventType::MessageStart,
            AgentEventType::MessageEnd,
            AgentEventType::TurnEnd,
            AgentEventType::AgentEnd
        ]
    );
    assert_eq!(agent.state().await.messages.len(), 2);
}

#[tokio::test]
async fn sequential_calls_execute_in_source_order() {
    let executed = Arc::new(Mutex::new(vec![]));
    let seen = executed.clone();
    let tool = AgentTool::new("echo", "echo", Schema::default(), move |context| {
        let seen = seen.clone();
        async move {
            seen.lock().push(context.tool_call_id.clone());
            Ok(AgentToolResult::text(context.tool_call_id))
        }
    });
    let stream = scripted(vec![
        assistant(
            vec![call("a", "echo"), call("b", "echo")],
            StopReason::ToolUse,
        ),
        assistant(vec![ContentBlock::text("done")], StopReason::Stop),
    ]);
    let mut opts = options(stream, vec![tool]);
    opts.tool_execution = ToolExecutionMode::Sequential;
    let agent = Agent::new(opts);
    agent.prompt("go").await.unwrap();
    assert_eq!(*executed.lock(), ["a", "b"]);
    let state = agent.state().await;
    assert_eq!(result_at(&state, 2).tool_call_id, "a");
    assert_eq!(result_at(&state, 3).tool_call_id, "b");
}

#[tokio::test]
async fn parallel_results_preserve_source_order() {
    let make = |name: &'static str, delay| {
        AgentTool::new(name, name, Schema::default(), move |context| async move {
            tokio::time::sleep(delay).await;
            Ok(AgentToolResult::text(context.tool_call_id))
        })
    };
    let stream = scripted(vec![
        assistant(
            vec![call("slow", "slow"), call("fast", "fast")],
            StopReason::ToolUse,
        ),
        assistant(vec![ContentBlock::text("done")], StopReason::Stop),
    ]);
    let mut opts = options(
        stream,
        vec![
            make("slow", Duration::from_millis(40)),
            make("fast", Duration::from_millis(1)),
        ],
    );
    opts.tool_execution = ToolExecutionMode::Parallel;
    let agent = Agent::new(opts);
    let ends = Arc::new(Mutex::new(vec![]));
    let observed = ends.clone();
    let _sub = agent
        .subscribe_simple(move |event| {
            let observed = observed.clone();
            async move {
                if let AgentEvent::ToolExecutionEnd { tool_call_id, .. } = event {
                    observed.lock().push(tool_call_id);
                }
                Ok(())
            }
        })
        .await;
    agent.prompt("go").await.unwrap();
    assert_eq!(*ends.lock(), ["fast", "slow"]);
    let state = agent.state().await;
    assert_eq!(result_at(&state, 2).tool_call_id, "slow");
    assert_eq!(result_at(&state, 3).tool_call_id, "fast");
}

#[tokio::test]
async fn tool_context_receives_the_active_turn_model() {
    let active = Model {
        id: "active-model".into(),
        name: "Active Model".into(),
        api: "scripted".into(),
        provider: "scripted".into(),
        input: vec!["text".into(), "image".into()],
        ..Model::default()
    };
    let seen = Arc::new(Mutex::new(None::<Model>));
    let observed = seen.clone();
    let tool = AgentTool::new("inspect", "inspect", Schema::default(), move |context| {
        let observed = observed.clone();
        async move {
            *observed.lock() = context.model;
            Ok(AgentToolResult::text("ok"))
        }
    });
    let stream = scripted(vec![
        assistant(vec![call("inspect-1", "inspect")], StopReason::ToolUse),
        assistant(vec![ContentBlock::text("done")], StopReason::Stop),
    ]);
    let mut opts = options(stream, vec![tool]);
    opts.initial_state.model = active.clone();
    let agent = Agent::new(opts);
    agent.prompt("go").await.unwrap();
    assert_eq!(*seen.lock(), Some(active));
}

#[tokio::test]
async fn tool_context_receives_prepare_next_turn_model_updates() {
    let initial = Model {
        id: "initial-model".into(),
        name: "Initial Model".into(),
        api: "scripted".into(),
        provider: "scripted".into(),
        ..Model::default()
    };
    let updated = Model {
        id: "updated-model".into(),
        name: "Updated Model".into(),
        api: "scripted".into(),
        provider: "scripted".into(),
        input: vec!["text".into(), "image".into()],
        ..Model::default()
    };
    let seen = Arc::new(Mutex::new(Vec::new()));
    let observed = seen.clone();
    let tool = AgentTool::new("inspect", "inspect", Schema::default(), move |context| {
        let observed = observed.clone();
        async move {
            observed.lock().push(context.model.expect("active model").id);
            Ok(AgentToolResult::text("ok"))
        }
    });
    let stream = scripted(vec![
        assistant(vec![call("inspect-1", "inspect")], StopReason::ToolUse),
        assistant(vec![call("inspect-2", "inspect")], StopReason::ToolUse),
        assistant(vec![ContentBlock::text("done")], StopReason::Stop),
    ]);
    let mut opts = options(stream, vec![tool]);
    opts.initial_state.model = initial;
    opts.prepare_next_turn = Some(Arc::new(move |_| {
        Some(AgentLoopTurnUpdate {
            model: Some(updated.clone()),
            ..AgentLoopTurnUpdate::default()
        })
    }));
    let agent = Agent::new(opts);
    agent.prompt("go").await.unwrap();
    assert_eq!(*seen.lock(), ["initial-model", "updated-model"]);
}

#[tokio::test]
async fn blocked_unknown_failing_and_panicking_tools_become_errors() {
    let fail = AgentTool::new("fail", "fail", Schema::default(), |_| async {
        Err(anyhow!("failed"))
    });
    let panic = AgentTool::new("panic", "panic", Schema::default(), |_| async {
        panic!("exploded")
    });
    let blocked = AgentTool::new("blocked", "blocked", Schema::default(), |_| async {
        Ok(AgentToolResult::text("ran"))
    });
    let stream = scripted(vec![
        assistant(
            vec![
                call("u", "missing"),
                call("b", "blocked"),
                call("f", "fail"),
                call("p", "panic"),
            ],
            StopReason::ToolUse,
        ),
        assistant(vec![ContentBlock::text("recovered")], StopReason::Stop),
    ]);
    let mut opts = options(stream, vec![fail, panic, blocked]);
    opts.tool_execution = ToolExecutionMode::Sequential;
    opts.before_tool_call = Some(Arc::new(|context| {
        Box::pin(async move {
            Ok(BeforeToolCallResult {
                block: context.tool_call.name == "blocked",
                reason: Some("denied".into()),
                arguments: None,
            })
        })
    }));
    let agent = Agent::new(opts);
    agent.prompt("go").await.unwrap();
    let state = agent.state().await;
    for index in 2..6 {
        assert!(result_at(&state, index).is_error);
    }
    assert_eq!(
        result_at(&state, 3).content,
        vec![ContentBlock::text("denied")]
    );
    assert_eq!(
        result_at(&state, 4).content,
        vec![ContentBlock::text("failed")]
    );
    assert_eq!(
        result_at(&state, 5).content,
        vec![ContentBlock::text("exploded")]
    );
}

#[tokio::test]
async fn steering_and_follow_up_are_injected() {
    let contexts = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
    let seen = contexts.clone();
    let messages = Arc::new(Mutex::new(VecDeque::from(vec![
        assistant(vec![ContentBlock::text("one")], StopReason::Stop),
        assistant(vec![ContentBlock::text("two")], StopReason::Stop),
        assistant(vec![ContentBlock::text("three")], StopReason::Stop),
    ])));
    let stream: StreamFn = Arc::new(move |_model, context, _options| {
        let seen = seen.clone();
        let messages = messages.clone();
        Box::pin(async move {
            seen.lock().push(
                context
                    .messages
                    .iter()
                    .filter_map(|message| {
                        if let Message::User(user) = message {
                            user.content.first().and_then(|block| {
                                if let ContentBlock::Text { text, .. } = block {
                                    Some(text.clone())
                                } else {
                                    None
                                }
                            })
                        } else {
                            None
                        }
                    })
                    .collect(),
            );
            let message = messages.lock().pop_front().unwrap();
            let stream = new_assistant_message_event_stream();
            stream
                .push(AssistantMessageEvent::Done {
                    reason: message.stop_reason,
                    message: message.clone(),
                })
                .await;
            stream.end(Some(message)).await;
            stream
        })
    });
    let mut opts = options(stream, vec![]);
    opts.steering_mode = QueueMode::OneAtATime;
    opts.follow_up_mode = QueueMode::OneAtATime;
    let agent = Agent::new(opts);
    agent
        .steer(Message::user_text("steer", pi_ai::now_millis()))
        .await;
    agent
        .follow_up(Message::user_text("follow", pi_ai::now_millis()))
        .await;
    agent.prompt("prompt").await.unwrap();
    let contexts = contexts.lock();
    assert_eq!(contexts.len(), 2);
    assert!(contexts[0].contains(&"steer".into()));
    assert!(contexts[1].contains(&"follow".into()));
}

#[tokio::test]
async fn queue_modes_and_pending_count_follow_mutations() {
    let agent = Agent::new(options(scripted(Vec::new()), vec![]));
    assert_eq!(agent.steering_mode().await, QueueMode::OneAtATime);
    assert_eq!(agent.follow_up_mode().await, QueueMode::OneAtATime);
    assert_eq!(agent.pending_message_count().await, 0);

    agent.set_steering_mode(QueueMode::All).await;
    agent.set_follow_up_mode(QueueMode::All).await;
    agent.steer(Message::user_text("steer", 1)).await;
    agent.follow_up(Message::user_text("follow", 2)).await;

    assert_eq!(agent.steering_mode().await, QueueMode::All);
    assert_eq!(agent.follow_up_mode().await, QueueMode::All);
    assert_eq!(agent.pending_message_count().await, 2);
    let (steering, follow_up) = agent.queued_messages().await;
    assert_eq!(steering, [Message::user_text("steer", 1)]);
    assert_eq!(follow_up, [Message::user_text("follow", 2)]);
    let (steering, follow_up) = agent.drain_queued_messages().await;
    assert_eq!(steering, [Message::user_text("steer", 1)]);
    assert_eq!(follow_up, [Message::user_text("follow", 2)]);
    assert_eq!(agent.pending_message_count().await, 0);
    agent.steer(Message::user_text("steer", 3)).await;
    agent.follow_up(Message::user_text("follow", 4)).await;
    agent.clear_steering_queue().await;
    assert_eq!(agent.pending_message_count().await, 1);
    agent.reset().await;
    assert_eq!(agent.pending_message_count().await, 0);
    assert_eq!(agent.steering_mode().await, QueueMode::All);
    assert_eq!(agent.follow_up_mode().await, QueueMode::All);
}

#[tokio::test]
async fn live_stream_options_apply_to_the_next_turn() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let observed = seen.clone();
    let stream: StreamFn = Arc::new(move |_model, _context, options| {
        let observed = observed.clone();
        Box::pin(async move {
            observed.lock().push(options.stream.temperature);
            let message = assistant(vec![ContentBlock::text("done")], StopReason::Stop);
            let stream = new_assistant_message_event_stream();
            stream
                .push(AssistantMessageEvent::Done {
                    reason: message.stop_reason,
                    message: message.clone(),
                })
                .await;
            stream.end(Some(message)).await;
            stream
        })
    });
    let agent = Agent::new(options(stream, vec![]));

    let mut first = agent.stream_options().await;
    first.stream.temperature = Some(0.2);
    agent.set_stream_options(first).await;
    agent.prompt("first").await.expect("first turn");

    let mut second = agent.stream_options().await;
    second.stream.temperature = Some(0.8);
    agent.set_stream_options(second).await;
    agent.prompt("second").await.expect("second turn");

    assert_eq!(*seen.lock(), vec![Some(0.2), Some(0.8)]);
}

#[tokio::test]
async fn ergonomic_stream_options_forward_to_the_canonical_snapshot() {
    let agent = Agent::new(options(scripted(vec![]), vec![]));

    agent.set_session_id(Some("session-42".to_owned())).await;
    agent
        .set_thinking_budgets(Some(ThinkingBudgets {
            high: Some(8_192),
            ..ThinkingBudgets::default()
        }))
        .await;
    agent.set_transport(Transport::WebSocketCached).await;
    agent.set_max_retry_delay(Some(12_345)).await;

    assert_eq!(agent.session_id().await.as_deref(), Some("session-42"));
    assert_eq!(agent.thinking_budgets().await.unwrap().high, Some(8_192));
    assert_eq!(agent.transport().await, Transport::WebSocketCached);
    assert_eq!(agent.max_retry_delay().await, Some(12_345));

    let snapshot = agent.stream_options().await;
    assert_eq!(snapshot.stream.session_id.as_deref(), Some("session-42"));
    assert_eq!(snapshot.thinking_budgets.unwrap().high, Some(8_192));
    assert_eq!(snapshot.stream.transport, Transport::WebSocketCached);
    assert_eq!(snapshot.stream.max_retry_delay_ms, Some(12_345));

    let mut replacement = SimpleStreamOptions::default();
    replacement.stream.session_id = Some("replacement".to_owned());
    agent.set_stream_options(replacement).await;
    assert_eq!(agent.session_id().await.as_deref(), Some("replacement"));
    assert_eq!(agent.transport().await, Transport::Sse);
    assert_eq!(agent.max_retry_delay().await, None);
}

#[tokio::test]
async fn terminal_provider_error_returns_original_error_without_duplicate_lifecycle() {
    let mut terminal = assistant(vec![ContentBlock::text("failed")], StopReason::Error);
    terminal.error_message = Some("provider failed".into());
    let agent = Agent::new(options(scripted(vec![terminal]), vec![]));
    let events = Arc::new(Mutex::new(Vec::new()));
    let observed = events.clone();
    let _sub = agent
        .subscribe_simple(move |event| {
            let observed = observed.clone();
            async move {
                observed.lock().push(event.event_type());
                Ok(())
            }
        })
        .await;

    let error = agent.prompt("go").await.unwrap_err();

    assert_eq!(error.to_string(), "provider failed");
    let state = agent.state().await;
    assert!(!state.is_streaming);
    assert_eq!(state.messages.len(), 2);
    assert!(matches!(state.messages.first(), Some(Message::User(_))));
    assert!(matches!(
        state.messages.last(),
        Some(Message::Assistant(message))
            if message.stop_reason == StopReason::Error
                && message.error_message.as_deref() == Some("provider failed")
    ));
    let events = events.lock();
    assert_eq!(
        *events,
        [
            AgentEventType::AgentStart,
            AgentEventType::TurnStart,
            AgentEventType::MessageStart,
            AgentEventType::MessageEnd,
            AgentEventType::MessageStart,
            AgentEventType::MessageEnd,
            AgentEventType::TurnEnd,
            AgentEventType::AgentEnd,
        ]
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, AgentEventType::TurnEnd))
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, AgentEventType::AgentEnd))
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, AgentEventType::MessageStart))
            .count(),
        2
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, AgentEventType::MessageEnd))
            .count(),
        2
    );
}

#[tokio::test]
async fn abort_terminates_stream_and_reaches_idle() {
    let started = Arc::new(Notify::new());
    let stream_started = started.clone();
    let stream: StreamFn = Arc::new(move |model, _context, options| {
        let stream_started = stream_started.clone();
        Box::pin(async move {
            let stream = new_assistant_message_event_stream();
            let producer = stream.clone();
            tokio::spawn(async move {
                stream_started.notify_waiters();
                options.stream.abort_signal.unwrap().cancelled().await;
                let mut message = AssistantMessage::pending(&model);
                message.stop_reason = StopReason::Aborted;
                message.error_message = Some("aborted".into());
                producer
                    .push(AssistantMessageEvent::Error {
                        reason: StopReason::Aborted,
                        error: message.clone(),
                    })
                    .await;
                producer.end(Some(message)).await;
            });
            stream
        })
    });
    let agent = Agent::new(options(stream, vec![]));
    let events = Arc::new(Mutex::new(Vec::new()));
    let observed = events.clone();
    let _sub = agent
        .subscribe_simple(move |event| {
            let observed = observed.clone();
            async move {
                observed.lock().push(event);
                Ok(())
            }
        })
        .await;
    let running = agent.clone();
    let prompt = tokio::spawn(async move { running.prompt("wait").await });
    started.notified().await;
    agent.abort().await;
    let error = prompt.await.unwrap().unwrap_err();
    assert_eq!(error.to_string(), "aborted");
    agent.wait_for_idle().await;
    let state = agent.state().await;
    assert!(!state.is_streaming);
    assert_eq!(state.messages.len(), 2);
    assert!(matches!(state.messages.first(), Some(Message::User(_))));
    assert!(
        matches!(state.messages.last(),Some(Message::Assistant(message))if message.stop_reason==StopReason::Aborted&&message.error_message.as_deref()==Some("aborted"))
    );
    let events = events.lock();
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, AgentEvent::TurnEnd { .. }))
            .count(),
        1
    );
    assert_eq!(events.iter().filter(|event|matches!(event,AgentEvent::MessageEnd{message:Message::Assistant(message)}if message.stop_reason==StopReason::Aborted)).count(),1);
}
#[tokio::test]
async fn listener_error_emits_failure_lifecycle() {
    let agent = Agent::new(options(
        scripted(vec![assistant(
            vec![ContentBlock::text("ok")],
            StopReason::Stop,
        )]),
        vec![],
    ));
    let failure_turns = Arc::new(Mutex::new(0));
    let observed = failure_turns.clone();
    let _sub = agent
        .subscribe_simple(move |event| {
            let observed = observed.clone();
            async move {
                match event {
                    AgentEvent::MessageEnd {
                        message: Message::Assistant(message),
                    } if message.stop_reason == StopReason::Stop => Err(anyhow!("listener failed")),
                    AgentEvent::TurnEnd { message, .. }
                        if message.stop_reason == StopReason::Error =>
                    {
                        *observed.lock() += 1;
                        Ok(())
                    }
                    _ => Ok(()),
                }
            }
        })
        .await;
    let error = agent.prompt("go").await.unwrap_err();
    assert!(error.to_string().contains("listener failed"));
    assert_eq!(*failure_turns.lock(), 1);
    let state = agent.state().await;
    assert_eq!(state.error_message.as_deref(), Some("listener failed"));
    assert!(!state.is_streaming);
    assert_eq!(state.messages.iter().filter(|message|matches!(message,Message::Assistant(message)if message.stop_reason==StopReason::Error)).count(),1);
}

#[tokio::test]
async fn execution_panic_emits_failure_lifecycle() {
    let stream: StreamFn =
        Arc::new(|_model, _context, _options| Box::pin(async move { panic!("stream panicked") }));
    let agent = Agent::new(options(stream, vec![]));
    let events = Arc::new(Mutex::new(Vec::new()));
    let observed = events.clone();
    let _sub = agent
        .subscribe_simple(move |event| {
            let observed = observed.clone();
            async move {
                observed.lock().push(event.event_type());
                Ok(())
            }
        })
        .await;

    let error = agent.prompt("go").await.unwrap_err();

    assert_eq!(error.to_string(), "stream panicked");
    let state = agent.state().await;
    assert!(!state.is_streaming);
    assert_eq!(state.messages.len(), 2);
    assert!(matches!(state.messages.first(), Some(Message::User(_))));
    assert!(matches!(
        state.messages.last(),
        Some(Message::Assistant(message))
            if message.stop_reason == StopReason::Error
                && message.error_message.as_deref() == Some("stream panicked")
    ));
    let events = events.lock();
    assert_eq!(
        *events,
        [
            AgentEventType::AgentStart,
            AgentEventType::TurnStart,
            AgentEventType::MessageStart,
            AgentEventType::MessageEnd,
            AgentEventType::MessageStart,
            AgentEventType::MessageEnd,
            AgentEventType::TurnEnd,
            AgentEventType::AgentEnd,
        ]
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, AgentEventType::TurnEnd))
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, AgentEventType::AgentEnd))
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, AgentEventType::MessageStart))
            .count(),
        2
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, AgentEventType::MessageEnd))
            .count(),
        2
    );
}

#[tokio::test]
async fn panicking_listener_becomes_run_error_and_session_stays_reusable() {
    let agent = Agent::new(options(
        scripted(vec![assistant(
            vec![ContentBlock::text("ok")],
            StopReason::Stop,
        )]),
        vec![],
    ));
    let later_calls = Arc::new(Mutex::new(0));
    let panic_sub = agent
        .subscribe_simple(|event| async move {
            if matches!(
                event,
                AgentEvent::MessageEnd { message: Message::Assistant(message) }
                    if message.stop_reason == StopReason::Stop
            ) {
                panic!("listener panicked");
            }
            Ok(())
        })
        .await;
    let observed = later_calls.clone();
    let _later = agent
        .subscribe_simple(move |event| {
            let observed = observed.clone();
            async move {
                if matches!(
                    event,
                    AgentEvent::MessageEnd {
                        message: Message::Assistant(_)
                    }
                ) {
                    *observed.lock() += 1;
                }
                Ok(())
            }
        })
        .await;

    let error = agent.prompt("go").await.unwrap_err();

    assert_eq!(error.to_string(), "listener panicked");
    // The later listener still ran for both the Stop MessageEnd and the
    // synthetic failure MessageEnd, despite the earlier listener panicking.
    assert_eq!(*later_calls.lock(), 2);
    // No panic escaped: the run reached idle without wedging active/is_streaming.
    agent.wait_for_idle().await;
    let state = agent.state().await;
    assert!(!state.is_streaming);
    assert_eq!(state.error_message.as_deref(), Some("listener panicked"));
    assert_eq!(
        state
            .messages
            .iter()
            .filter(|message| matches!(message, Message::Assistant(message) if message.stop_reason == StopReason::Error))
            .count(),
        1
    );

    // Dropping the panicking subscription removes only that listener; the
    // session remains usable for a subsequent prompt.
    drop(panic_sub);
    agent.prompt("again").await.unwrap();
    agent.wait_for_idle().await;
    let state = agent.state().await;
    assert!(!state.is_streaming);
    assert!(state.error_message.is_none());
}

#[tokio::test]
async fn terminal_only_stream_still_emits_message_start_before_end() {
    let stream: StreamFn = Arc::new(|_model, _context, _options| {
        Box::pin(async move {
            let message = assistant(vec![ContentBlock::text("terminal")], StopReason::Stop);
            let stream = new_assistant_message_event_stream();
            stream
                .push(AssistantMessageEvent::Done {
                    reason: StopReason::Stop,
                    message: message.clone(),
                })
                .await;
            stream.end(Some(message)).await;
            stream
        })
    });
    let agent = Agent::new(options(stream, vec![]));
    let lifecycle = Arc::new(Mutex::new(Vec::new()));
    let observed = lifecycle.clone();
    let _sub = agent
        .subscribe_simple(move |event| {
            let observed = observed.clone();
            async move {
                if matches!(
                    event,
                    AgentEvent::MessageStart {
                        message: Message::Assistant(_)
                    } | AgentEvent::MessageEnd {
                        message: Message::Assistant(_)
                    }
                ) {
                    observed.lock().push(event.event_type());
                }
                Ok(())
            }
        })
        .await;

    agent.prompt("go").await.unwrap();

    assert_eq!(
        *lifecycle.lock(),
        [AgentEventType::MessageStart, AgentEventType::MessageEnd]
    );
}

#[tokio::test]
async fn terminal_provider_error_preserves_error_and_completes_all_terminal_listeners() {
    let mut terminal = assistant(vec![ContentBlock::text("failed")], StopReason::Error);
    terminal.error_message = Some("provider failed".into());
    let agent = Agent::new(options(scripted(vec![terminal]), vec![]));
    let events = Arc::new(Mutex::new(Vec::new()));
    let observed = events.clone();
    let _record = agent
        .subscribe_simple(move |event| {
            let observed = observed.clone();
            async move {
                observed.lock().push(event.event_type());
                Ok(())
            }
        })
        .await;
    let _fail = agent
        .subscribe_simple(|event| async move {
            match event {
                AgentEvent::MessageEnd { message: Message::Assistant(message) }
                    if message.stop_reason == StopReason::Error => Err(anyhow!("message end listener")),
                AgentEvent::TurnEnd { message, .. } if message.stop_reason == StopReason::Error => {
                    Err(anyhow!("turn end listener"))
                }
                AgentEvent::AgentEnd { messages }
                    if matches!(messages.last(), Some(Message::Assistant(message)) if message.stop_reason == StopReason::Error) => {
                    Err(anyhow!("agent end listener"))
                }
                _ => Ok(()),
            }
        })
        .await;

    let error = agent.prompt("go").await.unwrap_err();

    assert_eq!(error.to_string(), "provider failed");
    let terminal_events: Vec<_> = events
        .lock()
        .iter()
        .copied()
        .filter(|event| {
            matches!(
                event,
                AgentEventType::MessageStart
                    | AgentEventType::MessageEnd
                    | AgentEventType::TurnEnd
                    | AgentEventType::AgentEnd
            )
        })
        .collect();
    assert_eq!(
        terminal_events,
        [
            AgentEventType::MessageStart,
            AgentEventType::MessageEnd,
            AgentEventType::MessageStart,
            AgentEventType::MessageEnd,
            AgentEventType::TurnEnd,
            AgentEventType::AgentEnd,
        ]
    );
    let state = agent.state().await;
    assert_eq!(state.messages.iter().filter(|message| matches!(message, Message::Assistant(message) if message.stop_reason == StopReason::Error)).count(), 1);
}

#[tokio::test]
async fn every_listener_runs_when_one_fails() {
    let agent = Agent::new(options(
        scripted(vec![assistant(
            vec![ContentBlock::text("ok")],
            StopReason::Stop,
        )]),
        vec![],
    ));
    let later_calls = Arc::new(Mutex::new(0));
    let _fail = agent
        .subscribe_simple(|event| async move {
            if matches!(
                event,
                AgentEvent::MessageEnd {
                    message: Message::Assistant(_)
                }
            ) {
                Err(anyhow!("first listener"))
            } else {
                Ok(())
            }
        })
        .await;
    let observed = later_calls.clone();
    let _later = agent
        .subscribe_simple(move |event| {
            let observed = observed.clone();
            async move {
                if matches!(
                    event,
                    AgentEvent::MessageEnd {
                        message: Message::Assistant(_)
                    }
                ) {
                    *observed.lock() += 1;
                }
                Ok(())
            }
        })
        .await;

    let error = agent.prompt("go").await.unwrap_err();

    assert_eq!(error.to_string(), "first listener");
    assert_eq!(*later_calls.lock(), 2);
}

#[tokio::test]
async fn synthetic_failure_preserves_first_listener_error_and_completes_lifecycle() {
    let agent = Agent::new(options(
        scripted(vec![assistant(
            vec![ContentBlock::text("ok")],
            StopReason::Stop,
        )]),
        vec![],
    ));
    let events = Arc::new(Mutex::new(Vec::new()));
    let observed = events.clone();
    let _listener = agent
        .subscribe_simple(move |event| {
            let observed = observed.clone();
            async move {
                observed.lock().push(event.clone());
                match event {
                    AgentEvent::MessageEnd { message: Message::Assistant(message) }
                        if message.stop_reason == StopReason::Stop => Err(anyhow!("initial listener")),
                    AgentEvent::MessageEnd { message: Message::Assistant(message) }
                        if message.stop_reason == StopReason::Error => Err(anyhow!("failure message listener")),
                    AgentEvent::TurnEnd { message, .. } if message.stop_reason == StopReason::Error => {
                        Err(anyhow!("failure turn listener"))
                    }
                    AgentEvent::AgentEnd { messages }
                        if matches!(messages.last(), Some(Message::Assistant(message)) if message.stop_reason == StopReason::Error) => {
                        Err(anyhow!("failure agent listener"))
                    }
                    _ => Ok(()),
                }
            }
        })
        .await;

    let error = agent.prompt("go").await.unwrap_err();

    assert_eq!(error.to_string(), "initial listener");
    let events = events.lock();
    let failure_events: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::MessageStart { message: Message::Assistant(message) }
                if message.stop_reason == StopReason::Error => Some(AgentEventType::MessageStart),
            AgentEvent::MessageEnd { message: Message::Assistant(message) }
                if message.stop_reason == StopReason::Error => Some(AgentEventType::MessageEnd),
            AgentEvent::TurnEnd { message, .. } if message.stop_reason == StopReason::Error => Some(AgentEventType::TurnEnd),
            AgentEvent::AgentEnd { messages }
                if matches!(messages.last(), Some(Message::Assistant(message)) if message.stop_reason == StopReason::Error) => Some(AgentEventType::AgentEnd),
            _ => None,
        })
        .collect();
    assert_eq!(
        failure_events,
        [
            AgentEventType::MessageStart,
            AgentEventType::MessageEnd,
            AgentEventType::TurnEnd,
            AgentEventType::AgentEnd
        ]
    );
    drop(events);
    let state = agent.state().await;
    assert_eq!(state.error_message.as_deref(), Some("initial listener"));
    assert_eq!(state.messages.iter().filter(|message| matches!(message, Message::Assistant(message) if message.stop_reason == StopReason::Error)).count(), 1);
}

#[tokio::test]
async fn result_only_error_emits_message_lifecycle_before_terminal_events() {
    let stream: StreamFn = Arc::new(|_model, _context, _options| {
        Box::pin(async move {
            let mut message = assistant(vec![ContentBlock::text("result only")], StopReason::Error);
            message.error_message = Some("result failed".into());
            let stream = new_assistant_message_event_stream();
            stream.end(Some(message)).await;
            stream
        })
    });
    let agent = Agent::new(options(stream, vec![]));
    let events = Arc::new(Mutex::new(Vec::new()));
    let observed = events.clone();
    let _sub = agent
        .subscribe_simple(move |event| {
            let observed = observed.clone();
            async move {
                if match &event {
                    AgentEvent::MessageStart { message: Message::Assistant(message) }
                    | AgentEvent::MessageEnd { message: Message::Assistant(message) }
                    | AgentEvent::TurnEnd { message, .. } => message.stop_reason == StopReason::Error,
                    AgentEvent::AgentEnd { messages } => matches!(messages.last(), Some(Message::Assistant(message)) if message.stop_reason == StopReason::Error),
                    _ => false,
                } {
                    observed.lock().push(event.event_type());
                }
                Ok(())
            }
        })
        .await;

    let error = agent.prompt("go").await.unwrap_err();

    assert_eq!(error.to_string(), "result failed");
    assert_eq!(
        *events.lock(),
        [
            AgentEventType::MessageStart,
            AgentEventType::MessageEnd,
            AgentEventType::TurnEnd,
            AgentEventType::AgentEnd
        ]
    );
    let state = agent.state().await;
    assert_eq!(state.messages.iter().filter(|message| matches!(message, Message::Assistant(message) if message.stop_reason == StopReason::Error)).count(), 1);
}

#[tokio::test]
async fn wait_for_idle_publishes_non_streaming_state_before_returning() {
    let stream: StreamFn = Arc::new(|_model, _context, _options| {
        Box::pin(async move {
            let stream = new_assistant_message_event_stream();
            stream
                .end(Some(assistant(
                    vec![ContentBlock::text("done")],
                    StopReason::Stop,
                )))
                .await;
            stream
        })
    });
    let agent = Agent::new(options(stream, vec![]));
    let started = Arc::new(Barrier::new(2));
    let release = Arc::new(Notify::new());
    let blocked = agent.clone();
    let blocked_started = started.clone();
    let blocked_release = release.clone();
    let _sub = agent
        .subscribe_simple(move |event| {
            let blocked_started = blocked_started.clone();
            let blocked_release = blocked_release.clone();
            async move {
                if matches!(event, AgentEvent::AgentEnd { .. }) {
                    blocked_started.wait().await;
                    blocked_release.notified().await;
                }
                Ok(())
            }
        })
        .await;
    let running = agent.clone();
    let prompt = tokio::spawn(async move { running.prompt("go").await });
    started.wait().await;
    let acquired = Arc::new(Notify::new());
    let state_release = Arc::new(Notify::new());
    let holder = {
        let acquired = acquired.clone();
        let state_release = state_release.clone();
        tokio::spawn(async move {
            blocked
                .hold_state_read_for_test(acquired, state_release)
                .await
        })
    };
    acquired.notified().await;
    release.notify_one();
    tokio::task::yield_now().await;
    assert!(
        tokio::time::timeout(Duration::from_millis(20), agent.wait_for_idle())
            .await
            .is_err()
    );
    state_release.notify_one();
    holder.await.unwrap();
    prompt.await.unwrap().unwrap();
    agent.wait_for_idle().await;
    assert!(!agent.state().await.is_streaming);
}

#[tokio::test]
async fn follow_up_only_continue_does_not_skip_new_steering() {
    let contexts = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
    let seen = contexts.clone();
    let stream: StreamFn = Arc::new(move |_model, context, _options| {
        let seen = seen.clone();
        Box::pin(async move {
            seen.lock().push(
                context
                    .messages
                    .iter()
                    .filter_map(|message| match message {
                        Message::User(user) => user.content.first().and_then(|block| match block {
                            ContentBlock::Text { text, .. } => Some(text.clone()),
                            _ => None,
                        }),
                        _ => None,
                    })
                    .collect(),
            );
            let message = assistant(vec![ContentBlock::text("done")], StopReason::Stop);
            let stream = new_assistant_message_event_stream();
            stream
                .push(AssistantMessageEvent::Done {
                    reason: message.stop_reason,
                    message: message.clone(),
                })
                .await;
            stream.end(Some(message)).await;
            stream
        })
    });
    let agent = Agent::new(options(stream, vec![]));
    agent
        .set_messages(vec![Message::Assistant(assistant(
            vec![ContentBlock::text("prior")],
            StopReason::Stop,
        ))])
        .await;
    agent
        .follow_up(Message::user_text("follow", pi_ai::now_millis()))
        .await;
    let steering_agent = agent.clone();
    let _sub = agent
        .subscribe_simple(move |event| {
            let steering_agent = steering_agent.clone();
            async move {
                if matches!(event, AgentEvent::AgentStart) {
                    steering_agent
                        .steer(Message::user_text("steer", pi_ai::now_millis()))
                        .await;
                }
                Ok(())
            }
        })
        .await;

    agent.continue_run().await.unwrap();

    assert_eq!(
        contexts.lock().as_slice(),
        &[vec!["follow".to_owned(), "steer".to_owned()]]
    );
}

/// A stream that, on its first invocation, parks inside a detached provider
/// task until the run's abort signal fires (notifying `started` once entered,
/// recording abort observation), then emits an `Aborted` terminal message. On
/// every subsequent invocation it returns a normal `Stop` message. This lets a
/// single agent exercise a dropped first run and then a successful reuse.
fn drop_safe_stream(started: Arc<Notify>, abort_observed: Arc<AtomicBool>) -> StreamFn {
    let call = Arc::new(AtomicUsize::new(0));
    Arc::new(move |model, _context, options| {
        let started = started.clone();
        let abort_observed = abort_observed.clone();
        let call = call.clone();
        Box::pin(async move {
            let first = call.fetch_add(1, Ordering::SeqCst) == 0;
            let stream = new_assistant_message_event_stream();
            if first {
                let producer = stream.clone();
                tokio::spawn(async move {
                    started.notify_waiters();
                    options.stream.abort_signal.unwrap().cancelled().await;
                    abort_observed.store(true, Ordering::SeqCst);
                    let mut message = AssistantMessage::pending(&model);
                    message.stop_reason = StopReason::Aborted;
                    message.error_message = Some("aborted".into());
                    producer
                        .push(AssistantMessageEvent::Error {
                            reason: StopReason::Aborted,
                            error: message.clone(),
                        })
                        .await;
                    producer.end(Some(message)).await;
                });
            } else {
                let message = assistant(vec![ContentBlock::text("done")], StopReason::Stop);
                stream
                    .push(AssistantMessageEvent::Done {
                        reason: StopReason::Stop,
                        message: message.clone(),
                    })
                    .await;
                stream.end(Some(message)).await;
            }
            stream
        })
    })
}

async fn assert_provider_observed_abort(flag: &AtomicBool) {
    assert!(
        tokio::time::timeout(Duration::from_millis(500), async {
            while !flag.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .is_ok(),
        "provider did not observe abort after the run future was dropped"
    );
}

#[tokio::test]
async fn dropping_prompt_future_after_provider_entered_releases_claim_and_aborts_provider() {
    let started = Arc::new(Notify::new());
    let abort_observed = Arc::new(AtomicBool::new(false));
    let agent = Agent::new(options(
        drop_safe_stream(started.clone(), abort_observed.clone()),
        vec![],
    ));

    // Poll the prompt future until the provider has entered, then drop it by
    // aborting the task that owns it.
    let running = agent.clone();
    let prompt = tokio::spawn(async move { running.prompt("wait").await });
    started.notified().await;
    prompt.abort();
    let _ = prompt.await;

    // The dropped future's guard must release the claim and notify idle.
    agent.wait_for_idle().await;
    let state = agent.state().await;
    assert!(!state.is_streaming, "is_streaming must clear after drop");
    assert!(
        state.streaming_message.is_none(),
        "streaming_message must clear after drop"
    );
    assert!(
        state.pending_tool_calls.is_empty(),
        "pending_tool_calls must clear after drop"
    );

    // The detached provider stream must have observed the abort.
    assert_provider_observed_abort(&abort_observed).await;

    // A subsequent prompt must succeed on the same agent (claim was released).
    agent.prompt("again").await.unwrap();
    agent.wait_for_idle().await;
    let state = agent.state().await;
    assert!(!state.is_streaming);
    assert!(state.streaming_message.is_none());
}

#[tokio::test]
async fn signal_aware_listeners_receive_the_active_signal_in_subscription_order() {
    let agent = Agent::new(options(
        scripted(vec![assistant(
            vec![ContentBlock::text("ok")],
            StopReason::Stop,
        )]),
        vec![],
    ));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let first_calls = calls.clone();
    let _first = agent
        .subscribe(move |event, signal| {
            let calls = first_calls.clone();
            async move {
                if matches!(event, AgentEvent::AgentStart) {
                    calls.lock().push(("first", signal.is_aborted()));
                }
                Ok(())
            }
        })
        .await;
    let second_calls = calls.clone();
    let _second = agent
        .subscribe(move |event, signal| {
            let calls = second_calls.clone();
            async move {
                if matches!(event, AgentEvent::AgentStart) {
                    calls.lock().push(("second", signal.is_aborted()));
                }
                Ok(())
            }
        })
        .await;

    agent.prompt("go").await.unwrap();

    assert_eq!(*calls.lock(), [("first", false), ("second", false)]);
}

#[tokio::test]
async fn listener_signal_observes_abort_during_the_active_run() {
    let started = Arc::new(Notify::new());
    let stream_started = started.clone();
    let stream: StreamFn = Arc::new(move |model, _context, options| {
        let stream_started = stream_started.clone();
        Box::pin(async move {
            let stream = new_assistant_message_event_stream();
            let producer = stream.clone();
            tokio::spawn(async move {
                producer
                    .push(AssistantMessageEvent::Start {
                        partial: AssistantMessage::pending(&model),
                    })
                    .await;
                stream_started.notify_waiters();
                options
                    .stream
                    .abort_signal
                    .expect("run abort signal")
                    .cancelled()
                    .await;
                let mut message = AssistantMessage::pending(&model);
                message.stop_reason = StopReason::Aborted;
                message.error_message = Some("aborted".into());
                producer
                    .push(AssistantMessageEvent::Error {
                        reason: StopReason::Aborted,
                        error: message.clone(),
                    })
                    .await;
                producer.end(Some(message)).await;
            });
            stream
        })
    });
    let agent = Agent::new(options(stream, vec![]));
    let saw_aborted = Arc::new(AtomicBool::new(false));
    let observed = saw_aborted.clone();
    let _subscription = agent
        .subscribe(move |event, signal| {
            let observed = observed.clone();
            async move {
                if matches!(event, AgentEvent::AgentEnd { .. }) {
                    observed.store(signal.is_aborted(), Ordering::SeqCst);
                }
                Ok(())
            }
        })
        .await;
    let running = agent.clone();
    let prompt = tokio::spawn(async move { running.prompt("wait").await });

    started.notified().await;
    agent.abort().await;
    let _ = prompt.await;

    assert!(saw_aborted.load(Ordering::SeqCst));
}

#[tokio::test]
async fn dropping_continue_future_after_provider_entered_releases_claim_and_aborts_provider() {
    let started = Arc::new(Notify::new());
    let abort_observed = Arc::new(AtomicBool::new(false));
    let agent = Agent::new(options(
        drop_safe_stream(started.clone(), abort_observed.clone()),
        vec![],
    ));
    agent
        .set_messages(vec![Message::user_text("prior", pi_ai::now_millis())])
        .await;

    let running = agent.clone();
    let continue_run = tokio::spawn(async move { running.continue_run().await });
    started.notified().await;
    continue_run.abort();
    let _ = continue_run.await;

    agent.wait_for_idle().await;
    let state = agent.state().await;
    assert!(!state.is_streaming, "is_streaming must clear after drop");
    assert!(
        state.streaming_message.is_none(),
        "streaming_message must clear after drop"
    );
    assert!(
        state.pending_tool_calls.is_empty(),
        "pending_tool_calls must clear after drop"
    );

    assert_provider_observed_abort(&abort_observed).await;

    // Subsequent reuse after the dropped continue.
    agent.prompt("again").await.unwrap();
    agent.wait_for_idle().await;
    assert!(!agent.state().await.is_streaming);
}

#[tokio::test]
async fn dropping_during_failure_finalization_does_not_duplicate_events() {
    // A listener fails on the normal Stop MessageEnd, forcing the run into the
    // synthetic failure finalization path. A second branch of the same
    // listener blocks on the synthetic Error MessageEnd, letting us drop the
    // run future mid-failure-finalization.
    let agent = Agent::new(options(
        scripted(vec![assistant(
            vec![ContentBlock::text("ok")],
            StopReason::Stop,
        )]),
        vec![],
    ));
    let events: Arc<Mutex<Vec<AgentEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let block = Arc::new(Notify::new());
    let blocked = Arc::new(AtomicBool::new(false));
    let observed = events.clone();
    let block_listener = block.clone();
    let blocked_listener = blocked.clone();
    let _sub = agent
        .subscribe_simple(move |event| {
            let observed = observed.clone();
            let block = block_listener.clone();
            let blocked = blocked_listener.clone();
            async move {
                observed.lock().push(event.clone());
                match event {
                    AgentEvent::MessageEnd {
                        message: Message::Assistant(message),
                    } if message.stop_reason == StopReason::Stop => {
                        Err(anyhow!("listener failed on stop"))
                    }
                    AgentEvent::MessageEnd {
                        message: Message::Assistant(message),
                    } if message.stop_reason == StopReason::Error => {
                        blocked.store(true, Ordering::SeqCst);
                        block.notified().await;
                        Ok(())
                    }
                    _ => Ok(()),
                }
            }
        })
        .await;

    let running = agent.clone();
    let prompt = tokio::spawn(async move { running.prompt("go").await });
    // Wait until failure finalization has begun (the listener is parked on
    // the synthetic Error MessageEnd).
    assert!(
        tokio::time::timeout(Duration::from_millis(500), async {
            while !blocked.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .is_ok(),
        "failure finalization listener never blocked"
    );

    // Drop the run future while failure finalization is in progress.
    prompt.abort();
    let _ = prompt.await;

    // The guard must still reach idle despite the mid-finalization drop.
    agent.wait_for_idle().await;
    let state = agent.state().await;
    assert!(
        !state.is_streaming,
        "is_streaming must clear after drop during finalization"
    );

    let events = events.lock();
    let agent_ends = events
        .iter()
        .filter(|event| matches!(event, AgentEvent::AgentEnd { .. }))
        .count();
    let turn_ends = events
        .iter()
        .filter(|event| matches!(event, AgentEvent::TurnEnd { .. }))
        .count();
    let failure_ends = events
        .iter()
        .filter(|event| {
            matches!(
                event,
                AgentEvent::MessageEnd { message: Message::Assistant(message) }
                    if message.stop_reason == StopReason::Error
            )
        })
        .count();
    // The cleanup must not re-emit the synthetic failure lifecycle, so each
    // terminal event appears at most once and the single failure MessageEnd
    // emitted before the drop is the only one.
    assert!(agent_ends <= 1, "duplicate AgentEnd after drop: {events:?}");
    assert!(turn_ends <= 1, "duplicate TurnEnd after drop: {events:?}");
    assert_eq!(
        failure_ends, 1,
        "expected exactly one failure MessageEnd: {events:?}"
    );
}

#[tokio::test]
async fn stale_drop_cleanup_does_not_clear_a_newer_claim() {
    // Deterministically exercises the generation guard: a stale cleanup for an
    // old run must be a no-op once a newer claim has replaced it, rather than
    // aborting/clearing the newer run. Without the generation check,
    // `release_claim_owned(old)` would take the newer run's `active` slot,
    // clearing `is_streaming` and wedging the in-flight newer run.
    let agent = Agent::new(options(
        scripted(vec![assistant(
            vec![ContentBlock::text("ok")],
            StopReason::Stop,
        )]),
        vec![],
    ));

    // Claim run 1.
    let gen1 = agent.claim_run_for_test().await.unwrap();
    assert_eq!(agent.current_run_generation_for_test().await, Some(gen1));
    assert!(agent.state().await.is_streaming);

    // Release run 1 (simulates the normal completion / drop cleanup for gen1).
    agent.release_run_generation_for_test(gen1).await;
    assert!(agent.current_run_generation_for_test().await.is_none());
    assert!(!agent.state().await.is_streaming);

    // A newer claim replaces it.
    let gen2 = agent.claim_run_for_test().await.unwrap();
    assert_ne!(gen1, gen2, "generations must be unique per claim");
    assert_eq!(agent.current_run_generation_for_test().await, Some(gen2));
    assert!(agent.state().await.is_streaming);

    // A stale cleanup for the old generation must NOT touch the newer claim.
    agent.release_run_generation_for_test(gen1).await;
    assert_eq!(
        agent.current_run_generation_for_test().await,
        Some(gen2),
        "stale cleanup cleared the newer claim's active slot"
    );
    assert!(
        agent.state().await.is_streaming,
        "stale cleanup cleared is_streaming for the newer claim"
    );

    // Release run 2 to leave the agent idle and reusable.
    agent.release_run_generation_for_test(gen2).await;
    assert!(agent.current_run_generation_for_test().await.is_none());
    assert!(!agent.state().await.is_streaming);

    // A fresh real prompt still works on the same agent.
    agent.prompt("again").await.unwrap();
    agent.wait_for_idle().await;
    assert!(!agent.state().await.is_streaming);
}

#[tokio::test]
async fn cancellation_during_release_still_releases_the_claim() {
    // Deterministically cancels a run while its `release_run` is blocked
    // mid-release. The assistant-last/empty-queue continue path is used so the
    // run reaches `release_run` immediately after claiming. A test seam
    // (`release_gate`) parks `release_claim_owned` after it has verified
    // ownership but before it clears state, so the cancel lands while the
    // release is in flight and the `active` claim is still held.
    //
    // With the fix (await release, then disarm) the guard is still armed when
    // the cancel arrives, so its `Drop` spawns a cleanup that completes the
    // release once the gate is opened. With the old bug (disarm before await)
    // the guard would already be disarmed and the agent would be wedged with
    // `active` set forever.
    let agent = Agent::new(options(scripted(vec![]), vec![]));
    agent
        .set_messages(vec![Message::Assistant(assistant(
            vec![ContentBlock::text("prior")],
            StopReason::Stop,
        ))])
        .await;

    let parked = Arc::new(Notify::new());
    let gate = Arc::new(Notify::new());
    agent.set_release_gate_for_test(Some((parked.clone(), gate.clone())));

    let running = agent.clone();
    let continue_run = tokio::spawn(async move { running.continue_run().await });

    // Wait until the release is in flight: `release_claim_owned` has verified
    // ownership and parked at the gate, holding `active` and with
    // `is_streaming` still set. (We cannot poll `active` here because the
    // parked release holds it; the `parked` signal is the synchronization.)
    assert!(
        tokio::time::timeout(Duration::from_millis(500), parked.notified())
            .await
            .is_ok(),
        "continue_run never reached its parked release"
    );
    assert!(agent.state().await.is_streaming);

    // Cancel the run while its release is parked. The armed guard must take
    // over the release.
    continue_run.abort();
    let _ = continue_run.await;

    // Open the gate so the guard's cleanup can proceed.
    gate.notify_one();
    agent.wait_for_idle().await;
    assert!(
        agent.current_run_generation_for_test().await.is_none(),
        "active claim was not released after cancellation during release"
    );
    assert!(!agent.state().await.is_streaming);

    // Remove the test seam and confirm the agent is reusable.
    agent.set_release_gate_for_test(None);
    agent.prompt("again").await.unwrap();
    agent.wait_for_idle().await;
    assert!(!agent.state().await.is_streaming);
}

#[tokio::test]
async fn dropped_run_future_releases_claim_across_runtime_shutdown() {
    // The Agent is constructed outside any runtime, a claimed run is polled on
    // a *temporary* runtime, the run future is dropped mid-run, and the
    // temporary runtime is then dropped immediately. The guard's cleanup runs
    // on a dedicated thread with its own minimal current-thread runtime, so it
    // must complete and release the claim regardless of the original runtime
    // being gone. On the test (second) runtime the agent must be idle and
    // reusable.
    use std::thread;
    use tokio::runtime::Runtime;

    let started = Arc::new(Notify::new());
    let stream = drop_safe_stream(started.clone(), Arc::new(AtomicBool::new(false)));
    let agent = Agent::new(options(stream, vec![]));
    let agent_for_temp = agent.clone();
    let started_for_temp = started.clone();

    // Temporary-runtime phase runs on a dedicated OS thread so `block_on` is
    // not nested inside the test runtime.
    let temp_phase = thread::spawn(move || {
        let temp = Runtime::new().unwrap();
        let prompt = temp.spawn(async move { agent_for_temp.prompt("wait").await });
        // Drive the temp runtime until the provider has entered (claim held).
        temp.block_on(started_for_temp.notified());
        // Drop the run future mid-run by aborting its task, and wait for the
        // abort to be fully processed (guard `Drop` fires here).
        prompt.abort();
        let _ = temp.block_on(prompt);
        // Drop the runtime immediately afterwards.
        drop(temp);
    });
    temp_phase.join().expect("temp phase thread panicked");

    // The runtime that polled the run is gone. The guard's cleanup thread used
    // its own runtime, so the claim must be released regardless.
    agent.wait_for_idle().await;
    let state = agent.state().await;
    assert!(
        !state.is_streaming,
        "is_streaming must clear across runtime shutdown"
    );
    assert!(state.streaming_message.is_none());
    assert!(
        agent.current_run_generation_for_test().await.is_none(),
        "active claim must be released across runtime shutdown"
    );

    // The agent must be reusable on the second runtime.
    agent.prompt("again").await.unwrap();
    agent.wait_for_idle().await;
    assert!(!agent.state().await.is_streaming);
}

#[tokio::test]
async fn empty_duplicate_tool_call_ids_are_normalized_distinctly() {
    let executed = Arc::new(Mutex::new(vec![]));
    let seen = executed.clone();
    let tool = AgentTool::new("echo", "echo", Schema::default(), move |context| {
        let seen = seen.clone();
        async move {
            seen.lock().push(context.tool_call_id.clone());
            Ok(AgentToolResult::text(context.tool_call_id))
        }
    });
    let mut first = assistant(
        vec![call("", "echo"), call("", "echo")],
        StopReason::ToolUse,
    );
    first.timestamp = 1_700_000_000_000;
    let stream = scripted(vec![
        first,
        assistant(vec![ContentBlock::text("done")], StopReason::Stop),
    ]);
    let mut opts = options(stream, vec![tool]);
    opts.tool_execution = ToolExecutionMode::Sequential;
    let agent = Agent::new(opts);

    let lifecycle = Arc::new(Mutex::new(vec![]));
    let observed = lifecycle.clone();
    let _sub = agent
        .subscribe_simple(move |event| {
            let observed = observed.clone();
            async move {
                match event {
                    AgentEvent::ToolExecutionStart { tool_call_id, .. }
                    | AgentEvent::ToolExecutionEnd { tool_call_id, .. } => {
                        observed.lock().push(tool_call_id);
                    }
                    _ => {}
                }
                Ok(())
            }
        })
        .await;

    agent.prompt("go").await.unwrap();
    let state = agent.state().await;
    let ids = tool_call_ids(assistant_at(&state, 1));
    assert_eq!(ids.len(), 2);
    assert_ne!(ids[0], ids[1], "empty same-name IDs must become distinct");
    assert!(ids.iter().all(|id| is_synthesized_tool_call_id(id)));
    assert_eq!(*executed.lock(), ids);
    assert_eq!(result_at(&state, 2).tool_call_id, ids[0]);
    assert_eq!(result_at(&state, 3).tool_call_id, ids[1]);
    assert_eq!(
        *lifecycle.lock(),
        vec![
            ids[0].clone(),
            ids[0].clone(),
            ids[1].clone(),
            ids[1].clone()
        ]
    );

    // Deterministic: same inputs yield the same synthesized pair.
    let mut again = assistant(
        vec![call("", "echo"), call("", "echo")],
        StopReason::ToolUse,
    );
    again.timestamp = 1_700_000_000_000;
    let agent2 = Agent::new({
        let mut opts = options(
            scripted(vec![
                again,
                assistant(vec![ContentBlock::text("done")], StopReason::Stop),
            ]),
            vec![AgentTool::new(
                "echo",
                "echo",
                Schema::default(),
                |_| async { Ok(AgentToolResult::text("ok")) },
            )],
        );
        opts.tool_execution = ToolExecutionMode::Sequential;
        opts
    });
    agent2.prompt("go").await.unwrap();
    assert_eq!(tool_call_ids(assistant_at(&agent2.state().await, 1)), ids);
}

#[tokio::test]
async fn duplicate_nonempty_tool_call_id_preserves_first_and_replaces_rest() {
    let tool = AgentTool::new("echo", "echo", Schema::default(), |context| async move {
        Ok(AgentToolResult::text(context.tool_call_id))
    });
    let mut message = assistant(
        vec![call("same", "echo"), call("same", "echo")],
        StopReason::ToolUse,
    );
    message.timestamp = 42;
    let stream = scripted(vec![
        message,
        assistant(vec![ContentBlock::text("done")], StopReason::Stop),
    ]);
    let mut opts = options(stream, vec![tool]);
    opts.tool_execution = ToolExecutionMode::Sequential;
    let agent = Agent::new(opts);
    agent.prompt("go").await.unwrap();
    let state = agent.state().await;
    let ids = tool_call_ids(assistant_at(&state, 1));
    assert_eq!(ids[0], "same");
    assert_ne!(ids[1], "same");
    assert!(is_synthesized_tool_call_id(&ids[1]));
    assert_eq!(result_at(&state, 2).tool_call_id, "same");
    assert_eq!(result_at(&state, 3).tool_call_id, ids[1]);
}

#[tokio::test]
async fn tool_call_id_colliding_with_prior_turn_is_replaced() {
    let tool = AgentTool::new("echo", "echo", Schema::default(), |context| async move {
        Ok(AgentToolResult::text(context.tool_call_id))
    });
    let mut first = assistant(vec![call("shared", "echo")], StopReason::ToolUse);
    first.timestamp = 10;
    let mut second = assistant(vec![call("shared", "echo")], StopReason::ToolUse);
    second.timestamp = 20;
    let stream = scripted(vec![
        first,
        assistant(vec![ContentBlock::text("mid")], StopReason::Stop),
        second,
        assistant(vec![ContentBlock::text("done")], StopReason::Stop),
    ]);
    let mut opts = options(stream, vec![tool]);
    opts.tool_execution = ToolExecutionMode::Sequential;
    let agent = Agent::new(opts);
    agent.prompt("first").await.unwrap();
    agent.prompt("second").await.unwrap();
    let state = agent.state().await;

    let first_ids = tool_call_ids(assistant_at(&state, 1));
    assert_eq!(first_ids, vec!["shared".to_string()]);
    let second_assistant = state
        .messages
        .iter()
        .enumerate()
        .filter_map(|(index, message)| match message {
            Message::Assistant(assistant)
                if assistant
                    .content
                    .iter()
                    .any(|block| matches!(block, ContentBlock::ToolCall(_)))
                    && index > 1 =>
            {
                Some(assistant)
            }
            _ => None,
        })
        .next()
        .expect("second tool assistant");
    let second_ids = tool_call_ids(second_assistant);
    assert_eq!(second_ids.len(), 1);
    assert_ne!(second_ids[0], "shared");
    assert!(is_synthesized_tool_call_id(&second_ids[0]));
    let second_result = state
        .messages
        .iter()
        .filter_map(|message| match message {
            Message::ToolResult(result) if result.tool_call_id != "shared" => Some(result),
            _ => None,
        })
        .next()
        .expect("second tool result");
    assert_eq!(second_result.tool_call_id, second_ids[0]);
}

#[tokio::test]
async fn unique_valid_provider_tool_call_id_is_preserved() {
    let tool = AgentTool::new("echo", "echo", Schema::default(), |context| async move {
        Ok(AgentToolResult::text(context.tool_call_id))
    });
    let stream = scripted(vec![
        assistant(vec![call("call_abc1", "echo")], StopReason::ToolUse),
        assistant(vec![ContentBlock::text("done")], StopReason::Stop),
    ]);
    let agent = Agent::new(options(stream, vec![tool]));
    let lifecycle = Arc::new(Mutex::new(vec![]));
    let observed = lifecycle.clone();
    let _sub = agent
        .subscribe_simple(move |event| {
            let observed = observed.clone();
            async move {
                match event {
                    AgentEvent::ToolExecutionStart { tool_call_id, .. }
                    | AgentEvent::ToolExecutionEnd { tool_call_id, .. } => {
                        observed.lock().push(tool_call_id);
                    }
                    _ => {}
                }
                Ok(())
            }
        })
        .await;
    agent.prompt("go").await.unwrap();
    let state = agent.state().await;
    assert_eq!(
        tool_call_ids(assistant_at(&state, 1)),
        vec!["call_abc1".to_string()]
    );
    assert_eq!(result_at(&state, 2).tool_call_id, "call_abc1");
    assert_eq!(
        *lifecycle.lock(),
        vec!["call_abc1".to_string(), "call_abc1".to_string()]
    );
}
