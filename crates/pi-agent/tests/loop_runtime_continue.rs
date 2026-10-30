//! Focused integration tests for the public `run_agent_loop_continue` entry
//! point: context validation (empty / assistant-last rejection) and the
//! serialization of parallel tool `ToolExecutionUpdate` events with
//! `ToolExecutionEnd` finalization events through the shared finalization lock.
//!
//! These tests own no source files; they exercise the public `pi_agent` API
//! only and do not touch `agent.rs` or `tests.rs`.

use std::{
    collections::VecDeque,
    fs::{self, OpenOptions},
    io::Write,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::Result;
use parking_lot::Mutex;
use pi_agent::{
    AbortController, AgentContext, AgentEvent, AgentLoopConfig, AgentTool, AgentToolResult,
    EventSink, StreamFn, ToolExecutionMode, run_agent_loop_continue,
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
    call_with_arguments(id, name, json!({}))
}

fn call_with_arguments(id: &str, name: &str, arguments: serde_json::Value) -> ContentBlock {
    ContentBlock::ToolCall(ToolCall {
        id: id.into(),
        name: name.into(),
        arguments,
        thought_signature: None,
    })
}

/// A `StreamFn` that replays a queue of scripted assistant messages. Each call
/// pops the next message, emits a `Start` then a terminal (`Done`/`Error`)
/// event, and ends the stream with the final message.
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

fn recording_sink(log: Arc<Mutex<Vec<AgentEvent>>>) -> EventSink {
    Arc::new(move |event| {
        let log = log.clone();
        Box::pin(async move {
            log.lock().push(event);
            Ok(())
        })
    })
}

/// A listener that records `(tool_call_id, kind)` for tool execution events
/// and tracks how many listener invocations are in flight concurrently. The
/// `yield_now` gives concurrent emitters a chance to interleave; with the
/// finalization lock, only one emitter can be active at a time, so the
/// observed high-water mark stays at 1.
fn concurrency_sink(
    log: Arc<Mutex<Vec<(String, &'static str)>>>,
    in_flight: Arc<AtomicUsize>,
    max_in_flight: Arc<AtomicUsize>,
) -> EventSink {
    Arc::new(move |event| {
        let log = log.clone();
        let in_flight = in_flight.clone();
        let max_in_flight = max_in_flight.clone();
        Box::pin(async move {
            let count = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            let _ = max_in_flight.fetch_max(count, Ordering::SeqCst);
            tokio::task::yield_now().await;
            let record = match event {
                AgentEvent::ToolExecutionStart { tool_call_id, .. } => (tool_call_id, "start"),
                AgentEvent::ToolExecutionUpdate { tool_call_id, .. } => (tool_call_id, "update"),
                AgentEvent::ToolExecutionEnd { tool_call_id, .. } => (tool_call_id, "end"),
                _ => (String::new(), "other"),
            };
            log.lock().push(record);
            in_flight.fetch_sub(1, Ordering::SeqCst);
            Ok(())
        })
    })
}

fn empty_context() -> AgentContext {
    AgentContext {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: Vec::new(),
    }
}

#[tokio::test]
async fn continue_rejects_empty_context() {
    let (_controller, abort) = AbortController::new();
    let log = Arc::new(Mutex::new(Vec::new()));
    let emit = recording_sink(log.clone());
    let error = run_agent_loop_continue(
        empty_context(),
        AgentLoopConfig::default(),
        emit,
        scripted(Vec::new()),
        abort,
    )
    .await
    .unwrap_err();

    assert!(
        error.to_string().contains("empty context"),
        "expected an empty-context rejection, got: {error}"
    );
    assert!(
        log.lock().is_empty(),
        "a rejected continue must not emit any lifecycle events"
    );
}

#[tokio::test]
async fn continue_rejects_assistant_last_context() {
    let (_controller, abort) = AbortController::new();
    let context = AgentContext {
        system_prompt: String::new(),
        messages: vec![
            Message::user_text("q", 0),
            Message::Assistant(assistant(vec![ContentBlock::text("hi")], StopReason::Stop)),
        ],
        tools: Vec::new(),
    };
    let log = Arc::new(Mutex::new(Vec::new()));
    let emit = recording_sink(log.clone());
    let error = run_agent_loop_continue(
        context,
        AgentLoopConfig::default(),
        emit,
        scripted(Vec::new()),
        abort,
    )
    .await
    .unwrap_err();

    assert!(
        error.to_string().contains("assistant message"),
        "expected an assistant-last rejection, got: {error}"
    );
    assert!(
        log.lock().is_empty(),
        "a rejected continue must not emit any lifecycle events"
    );
}

#[tokio::test]
async fn continue_accepts_user_last_context() {
    let (_controller, abort) = AbortController::new();
    let context = AgentContext {
        system_prompt: String::new(),
        messages: vec![Message::user_text("go", 0)],
        tools: Vec::new(),
    };
    let log = Arc::new(Mutex::new(Vec::new()));
    let emit = recording_sink(log.clone());
    let new_messages = run_agent_loop_continue(
        context,
        AgentLoopConfig::default(),
        emit,
        scripted(vec![assistant(
            vec![ContentBlock::text("done")],
            StopReason::Stop,
        )]),
        abort,
    )
    .await
    .expect("a user-last context should continue without rejection");

    assert_eq!(new_messages.len(), 1);
    assert!(
        matches!(new_messages.last(), Some(Message::Assistant(_))),
        "the continued turn should append the streamed assistant message"
    );
    let log = log.lock();
    assert!(
        log.iter()
            .any(|event| matches!(event, AgentEvent::AgentStart))
    );
    assert!(
        log.iter()
            .any(|event| matches!(event, AgentEvent::AgentEnd { .. }))
    );
    assert!(
        !log.iter()
            .any(|event| matches!(event, AgentEvent::ToolExecutionEnd { .. })),
        "no tools were requested, so no tool end events should appear"
    );
}

#[tokio::test]
async fn output_limit_tool_call_settles_without_execution() {
    let executions = Arc::new(AtomicUsize::new(0));
    let observed = executions.clone();
    let tool = AgentTool::new("write", "write", Schema::default(), move |_| {
        let observed = observed.clone();
        async move {
            observed.fetch_add(1, Ordering::SeqCst);
            Ok(AgentToolResult::text("unexpected execution"))
        }
    });
    let stream = scripted(vec![
        assistant(vec![call("write-call", "write")], StopReason::Length),
        assistant(vec![ContentBlock::text("recovered")], StopReason::Stop),
    ]);
    let context = AgentContext {
        system_prompt: String::new(),
        messages: vec![Message::user_text("write the file", 0)],
        tools: vec![tool],
    };
    let log = Arc::new(Mutex::new(Vec::new()));
    let (_controller, abort) = AbortController::new();

    run_agent_loop_continue(
        context,
        AgentLoopConfig::default(),
        recording_sink(log.clone()),
        stream,
        abort,
    )
    .await
    .expect("the model loop should recover after the rejected call");

    assert_eq!(executions.load(Ordering::SeqCst), 0);
    let events = log.lock();
    assert!(events.iter().any(|event| matches!(event,
        AgentEvent::ToolExecutionEnd { tool_call_id, is_error: true, .. }
            if tool_call_id == "write-call"
    )));
    assert!(events.iter().any(|event| matches!(event,
        AgentEvent::TurnEnd { tool_results, .. }
            if tool_results.iter().any(|message| message.tool_call_id == "write-call" && message.is_error)
    )));
}

#[tokio::test]
async fn output_limit_write_recovers_through_bounded_chunks() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock after Unix epoch")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "pi-agent-write-recovery-{}-{unique}.txt",
        std::process::id()
    ));
    let executions = Arc::new(AtomicUsize::new(0));
    let observed_executions = executions.clone();
    let expected_path = path.clone();
    let write = AgentTool::new(
        "write",
        "write",
        Schema::object_ordered(vec![
            ("path".to_owned(), Schema::string(), true),
            ("content".to_owned(), Schema::string(), true),
            (
                "append".to_owned(),
                Schema {
                    schema_type: Some(json!("boolean")),
                    ..Schema::default()
                },
                true,
            ),
        ]),
        move |context| {
            let observed_executions = observed_executions.clone();
            let expected_path = expected_path.clone();
            async move {
                observed_executions.fetch_add(1, Ordering::SeqCst);
                let actual_path = context
                    .arguments
                    .get("path")
                    .and_then(serde_json::Value::as_str)
                    .expect("validated path");
                assert_eq!(std::path::Path::new(actual_path), expected_path);
                let content = context
                    .arguments
                    .get("content")
                    .and_then(serde_json::Value::as_str)
                    .expect("validated content");
                let append = context
                    .arguments
                    .get("append")
                    .and_then(serde_json::Value::as_bool)
                    .expect("validated append");
                let mut output = OpenOptions::new()
                    .create(true)
                    .write(true)
                    .append(append)
                    .truncate(!append)
                    .open(&expected_path)?;
                output.write_all(content.as_bytes())?;
                Ok(AgentToolResult::text("chunk written"))
            }
        },
    );
    let path_arg = path.to_string_lossy().into_owned();
    let stream = scripted(vec![
        assistant(
            vec![call_with_arguments(
                "oversized-write",
                "write",
                json!({ "path": path_arg, "content": "must not execute", "append": false }),
            )],
            StopReason::Length,
        ),
        assistant(
            vec![call_with_arguments(
                "write-first-chunk",
                "write",
                json!({ "path": path_arg, "content": "bounded-", "append": false }),
            )],
            StopReason::ToolUse,
        ),
        assistant(
            vec![call_with_arguments(
                "write-second-chunk",
                "write",
                json!({ "path": path_arg, "content": "content\n", "append": true }),
            )],
            StopReason::ToolUse,
        ),
        assistant(vec![ContentBlock::text("recovered")], StopReason::Stop),
    ]);
    let context = AgentContext {
        system_prompt: String::new(),
        messages: vec![Message::user_text("write the file", 0)],
        tools: vec![write],
    };
    let log = Arc::new(Mutex::new(Vec::new()));
    let (_controller, abort) = AbortController::new();

    run_agent_loop_continue(
        context,
        AgentLoopConfig::default(),
        recording_sink(log.clone()),
        stream,
        abort,
    )
    .await
    .expect("the model loop should recover through bounded writes");

    assert_eq!(executions.load(Ordering::SeqCst), 2);
    assert_eq!(
        fs::read(&path).expect("completed file"),
        b"bounded-content\n"
    );
    let events = log.lock();
    assert!(events.iter().any(|event| matches!(event,
        AgentEvent::ToolExecutionEnd { tool_call_id, is_error: true, .. }
            if tool_call_id == "oversized-write"
    )));
    for tool_call_id in ["write-first-chunk", "write-second-chunk"] {
        assert!(events.iter().any(|event| matches!(event,
            AgentEvent::ToolExecutionEnd { tool_call_id: id, is_error: false, .. }
                if id == tool_call_id
        )));
    }
    drop(events);
    fs::remove_file(&path).expect("remove completed file");
}

#[tokio::test]
async fn missing_required_command_settles_as_schema_error_without_execution() {
    let executions = Arc::new(AtomicUsize::new(0));
    let observed = executions.clone();
    let command = AgentTool::new(
        "bash",
        "bash",
        Schema::object_ordered(vec![("command".to_owned(), Schema::string(), true)]),
        move |_| {
            let observed = observed.clone();
            async move {
                observed.fetch_add(1, Ordering::SeqCst);
                Ok(AgentToolResult::text("unexpected execution"))
            }
        },
    );
    let stream = scripted(vec![
        assistant(vec![call("command-call", "bash")], StopReason::ToolUse),
        assistant(vec![ContentBlock::text("recovered")], StopReason::Stop),
    ]);
    let context = AgentContext {
        system_prompt: String::new(),
        messages: vec![Message::user_text("run command", 0)],
        tools: vec![command],
    };
    let log = Arc::new(Mutex::new(Vec::new()));
    let (_controller, abort) = AbortController::new();

    run_agent_loop_continue(
        context,
        AgentLoopConfig::default(),
        recording_sink(log.clone()),
        stream,
        abort,
    )
    .await
    .expect("the model loop should continue after the schema error");

    assert_eq!(executions.load(Ordering::SeqCst), 0);
    let events = log.lock();
    assert!(events.iter().any(|event| matches!(event,
        AgentEvent::ToolExecutionEnd { tool_call_id, is_error: true, .. }
            if tool_call_id == "command-call"
    )));
    assert!(events.iter().any(|event| matches!(event,
        AgentEvent::TurnEnd { tool_results, .. }
            if tool_results.iter().any(|message| message.tool_call_id == "command-call" && message.is_error)
    )));
}

#[tokio::test]
async fn parallel_update_and_end_events_are_serialized() {
    // "slow" emits two updates around a delay so it is still executing while
    // "fast" finalizes; both run in parallel.
    let slow = AgentTool::new("slow", "slow", Schema::default(), move |context| {
        let on_update = context.on_update.clone();
        async move {
            (on_update)(AgentToolResult::text("slow-1"));
            tokio::time::sleep(Duration::from_millis(30)).await;
            (on_update)(AgentToolResult::text("slow-2"));
            Ok(AgentToolResult::text(context.tool_call_id))
        }
    });
    let fast = AgentTool::new("fast", "fast", Schema::default(), move |context| {
        let on_update = context.on_update.clone();
        async move {
            (on_update)(AgentToolResult::text("fast-1"));
            Ok(AgentToolResult::text(context.tool_call_id))
        }
    });
    let stream = scripted(vec![
        assistant(
            vec![call("slow", "slow"), call("fast", "fast")],
            StopReason::ToolUse,
        ),
        assistant(vec![ContentBlock::text("done")], StopReason::Stop),
    ]);
    let context = AgentContext {
        system_prompt: String::new(),
        messages: vec![Message::user_text("go", 0)],
        tools: vec![slow, fast],
    };
    let mut config = AgentLoopConfig::default();
    config.tool_execution = ToolExecutionMode::Parallel;

    let log = Arc::new(Mutex::new(Vec::new()));
    let in_flight = Arc::new(AtomicUsize::new(0));
    let max_in_flight = Arc::new(AtomicUsize::new(0));
    let (_controller, abort) = AbortController::new();
    let emit = concurrency_sink(log.clone(), in_flight.clone(), max_in_flight.clone());

    run_agent_loop_continue(context, config, emit, stream, abort)
        .await
        .expect("the loop should complete");

    // Serialization contract: update and end events go through the shared
    // finalization lock, so the listener is never entered by two emitters
    // concurrently.
    assert_eq!(
        max_in_flight.load(Ordering::SeqCst),
        1,
        "parallel tool update and end events must be serialized through the lock"
    );

    let events = log.lock().clone();
    for tool in ["slow", "fast"] {
        let mut last_update = None;
        let mut end = None;
        for (index, (tool_call_id, kind)) in events.iter().enumerate() {
            if tool_call_id == tool {
                match *kind {
                    "update" => last_update = Some(index),
                    "end" => end = Some(index),
                    _ => {}
                }
            }
        }
        let end = end.unwrap_or_else(|| panic!("tool {tool} should emit a ToolExecutionEnd"));
        if let Some(last_update) = last_update {
            assert!(
                last_update < end,
                "tool {tool}: every update event must precede its own end event"
            );
        }
        assert!(
            events
                .iter()
                .filter(|(tool_call_id, kind)| tool_call_id == tool && *kind == "update")
                .count()
                >= 1,
            "tool {tool} should emit at least one update event"
        );
    }
}
