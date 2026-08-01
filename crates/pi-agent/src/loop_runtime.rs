use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use anyhow::{Result, anyhow};
use futures_util::{FutureExt, future::join_all};
use pi_ai::{
    AssistantMessage, AssistantMessageEvent, ContentBlock, Context, Message, StopReason, ToolCall,
    ToolResultMessage,
};
use tokio::sync::{Mutex, mpsc, oneshot};

use crate::{
    AbortSignal, AfterToolCallContext, AgentContext, AgentEvent, AgentLoopConfig, AgentMessage,
    AgentTool, AgentToolResult, BeforeToolCallContext, EventSink, ShouldStopAfterTurnContext,
    StreamFn, ThinkingLevel, ToolCallContext, ToolExecutionMode, ToolUpdateFn,
};

#[derive(Clone, Debug)]
struct FinalizedOutcome {
    tool_call: ToolCall,
    result: AgentToolResult,
    is_error: bool,
}

#[derive(Clone)]
struct PreparedToolCall {
    tool_call: ToolCall,
    tool: AgentTool,
    arguments: serde_json::Value,
    model: pi_ai::Model,
}

#[derive(Clone, Debug)]
struct ExecutedBatch {
    messages: Vec<ToolResultMessage>,
    terminate: bool,
}

pub async fn run_agent_loop(
    prompts: Vec<AgentMessage>,
    mut context: AgentContext,
    config: AgentLoopConfig,
    emit: EventSink,
    stream_fn: StreamFn,
    abort: AbortSignal,
) -> Result<Vec<AgentMessage>> {
    let mut new_messages = prompts.clone();
    context.messages.extend(prompts.clone());

    emit(AgentEvent::AgentStart).await?;
    emit(AgentEvent::TurnStart).await?;
    for prompt in prompts {
        emit(AgentEvent::MessageStart {
            message: prompt.clone(),
        })
        .await?;
        emit(AgentEvent::MessageEnd { message: prompt }).await?;
    }

    run_loop(
        &mut context,
        &mut new_messages,
        config,
        emit,
        stream_fn,
        abort,
        true,
    )
    .await?;
    Ok(new_messages)
}

pub async fn run_agent_loop_continue(
    mut context: AgentContext,
    config: AgentLoopConfig,
    emit: EventSink,
    stream_fn: StreamFn,
    abort: AbortSignal,
) -> Result<Vec<AgentMessage>> {
    // A continue must resume from a non-empty, non-assistant context: an empty
    // context has nothing to continue from, and an assistant-last context would
    // re-prompt the model with its own trailing turn still in place (callers
    // route that case through `run_agent_loop` with queued steering/follow-up
    // messages). Validate before emitting any lifecycle events so a rejected
    // continue leaves the listener stream untouched. The skip-initial-steering
    // flag and the rest of the steering semantics live in `run_loop`; this guard
    // only enforces the continue precondition and preserves them unchanged.
    match context.messages.last() {
        None => {
            return Err(anyhow!(
                "Cannot continue an agent loop from an empty context"
            ));
        }
        Some(Message::Assistant(_)) => {
            return Err(anyhow!(
                "Cannot continue an agent loop from an assistant message; \
                 provide a user or tool result message to resume from"
            ));
        }
        Some(_) => {}
    }

    let mut new_messages = Vec::new();
    emit(AgentEvent::AgentStart).await?;
    emit(AgentEvent::TurnStart).await?;
    run_loop(
        &mut context,
        &mut new_messages,
        config,
        emit,
        stream_fn,
        abort,
        true,
    )
    .await?;
    Ok(new_messages)
}

async fn run_loop(
    context: &mut AgentContext,
    new_messages: &mut Vec<AgentMessage>,
    mut config: AgentLoopConfig,
    emit: EventSink,
    stream_fn: StreamFn,
    abort: AbortSignal,
    mut first_turn: bool,
) -> Result<()> {
    let mut pending = if config.skip_initial_steering {
        Vec::new()
    } else if let Some(get) = &config.get_steering_messages {
        get().await
    } else {
        Vec::new()
    };

    loop {
        let mut has_more_tool_calls = true;
        while has_more_tool_calls || !pending.is_empty() {
            if first_turn {
                first_turn = false;
            } else {
                emit(AgentEvent::TurnStart).await?;
            }

            for message in pending.drain(..) {
                emit(AgentEvent::MessageStart {
                    message: message.clone(),
                })
                .await?;
                emit(AgentEvent::MessageEnd {
                    message: message.clone(),
                })
                .await?;
                context.messages.push(message.clone());
                new_messages.push(message);
            }

            let message = stream_assistant_response(
                context,
                &config,
                emit.clone(),
                stream_fn.clone(),
                abort.clone(),
            )
            .await?;
            new_messages.push(Message::Assistant(message.clone()));

            if matches!(message.stop_reason, StopReason::Error | StopReason::Aborted) {
                let terminal_error = message
                    .error_message
                    .clone()
                    .unwrap_or_else(|| format!("assistant stopped with {:?}", message.stop_reason));
                let _turn_listener_error = emit(AgentEvent::TurnEnd {
                    message,
                    tool_results: Vec::new(),
                })
                .await
                .err();
                let _agent_listener_error = emit(AgentEvent::AgentEnd {
                    messages: new_messages.clone(),
                })
                .await
                .err();
                return Err(anyhow!(terminal_error));
            }

            let tool_calls = tool_calls(&message);
            let batch = if tool_calls.is_empty() {
                ExecutedBatch {
                    messages: Vec::new(),
                    terminate: false,
                }
            } else if message.stop_reason == StopReason::Length {
                fail_truncated_tool_calls(&tool_calls, emit.clone()).await?
            } else {
                execute_tool_calls(context, &message, &config, emit.clone(), abort.clone()).await?
            };
            has_more_tool_calls = !tool_calls.is_empty() && !batch.terminate;
            for result in &batch.messages {
                let message = Message::ToolResult(result.clone());
                context.messages.push(message.clone());
                new_messages.push(message);
            }

            emit(AgentEvent::TurnEnd {
                message: message.clone(),
                tool_results: batch.messages.clone(),
            })
            .await?;

            let turn_context = ShouldStopAfterTurnContext {
                message,
                tool_results: batch.messages,
                context: context.clone(),
                new_messages: new_messages.clone(),
            };
            if let Some(prepare) = &config.prepare_next_turn {
                if let Some(update) = prepare(&turn_context) {
                    if let Some(updated_context) = update.context {
                        *context = updated_context;
                    }
                    if let Some(model) = update.model {
                        config.model = model;
                    }
                    if let Some(level) = update.thinking_level {
                        config.reasoning = level;
                    }
                }
            }
            if config
                .should_stop_after_turn
                .as_ref()
                .is_some_and(|should_stop| should_stop(&turn_context))
            {
                emit(AgentEvent::AgentEnd {
                    messages: new_messages.clone(),
                })
                .await?;
                return Ok(());
            }
            pending = if let Some(get) = &config.get_steering_messages {
                get().await
            } else {
                Vec::new()
            };
        }

        let follow_ups = if let Some(get) = &config.get_follow_up_messages {
            get().await
        } else {
            Vec::new()
        };
        if follow_ups.is_empty() {
            break;
        }
        pending = follow_ups;
    }

    emit(AgentEvent::AgentEnd {
        messages: new_messages.clone(),
    })
    .await?;
    Ok(())
}

async fn stream_assistant_response(
    context: &mut AgentContext,
    config: &AgentLoopConfig,
    emit: EventSink,
    stream_fn: StreamFn,
    abort: AbortSignal,
) -> Result<AssistantMessage> {
    let mut messages = context.messages.clone();
    if let Some(transform) = &config.transform_context {
        messages = transform(messages, abort.clone()).await?;
    }
    let messages = if let Some(convert) = &config.convert_to_llm {
        convert(messages)?
    } else {
        messages
    };
    let llm_context = Context {
        system_prompt: context.system_prompt.clone(),
        messages,
        tools: context
            .tools
            .iter()
            .map(AgentTool::as_tool_definition)
            .collect(),
    };
    let mut options = config.stream_options.clone();
    options.stream.abort_signal = Some(abort.cancellation_token());
    options.reasoning = match config.reasoning {
        ThinkingLevel::Off => None,
        ThinkingLevel::Minimal => Some(pi_ai::ThinkingLevel::Minimal),
        ThinkingLevel::Low => Some(pi_ai::ThinkingLevel::Low),
        ThinkingLevel::Medium => Some(pi_ai::ThinkingLevel::Medium),
        ThinkingLevel::High => Some(pi_ai::ThinkingLevel::High),
        ThinkingLevel::Xhigh => Some(pi_ai::ThinkingLevel::XHigh),
    };
    if let Some(get_api_key) = &config.get_api_key {
        if let Some(api_key) = get_api_key(&config.model.provider) {
            options.stream.api_key = Some(api_key);
        }
    }

    let stream = stream_fn(config.model.clone(), llm_context, options).await;
    let mut added_partial = false;
    while let Some(event) = stream.next().await {
        match &event {
            AssistantMessageEvent::Start { partial } => {
                context.messages.push(Message::Assistant(partial.clone()));
                added_partial = true;
                emit(AgentEvent::MessageStart {
                    message: Message::Assistant(partial.clone()),
                })
                .await?;
            }
            AssistantMessageEvent::Done { message, .. } => {
                replace_or_append_assistant(context, message.clone(), added_partial);
                emit_assistant_completion(message, added_partial, emit.clone()).await?;
                return Ok(message.clone());
            }
            AssistantMessageEvent::Error { error, .. } => {
                replace_or_append_assistant(context, error.clone(), added_partial);
                let _listener_error = emit_assistant_completion(error, added_partial, emit.clone())
                    .await
                    .err();
                return Ok(error.clone());
            }
            _ => {
                if let Some(partial) = event.partial() {
                    replace_or_append_assistant(context, partial.clone(), added_partial);
                    emit(AgentEvent::MessageUpdate {
                        message: Message::Assistant(partial.clone()),
                        assistant_message_event: event,
                    })
                    .await?;
                }
            }
        }
    }

    let message = stream
        .result()
        .await
        .ok_or_else(|| anyhow!("assistant stream ended without a final message"))?;
    replace_or_append_assistant(context, message.clone(), added_partial);
    let completion = emit_assistant_completion(&message, added_partial, emit).await;
    if matches!(message.stop_reason, StopReason::Error | StopReason::Aborted) {
        let _listener_error = completion.err();
    } else {
        completion?;
    }
    Ok(message)
}

async fn emit_assistant_completion(
    message: &AssistantMessage,
    started: bool,
    emit: EventSink,
) -> Result<()> {
    let mut first_error = None;
    if !started {
        if let Err(error) = emit(AgentEvent::MessageStart {
            message: Message::Assistant(message.clone()),
        })
        .await
        {
            first_error = Some(error);
        }
    }
    if let Err(error) = emit(AgentEvent::MessageEnd {
        message: Message::Assistant(message.clone()),
    })
    .await
    {
        if first_error.is_none() {
            first_error = Some(error);
        }
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn replace_or_append_assistant(
    context: &mut AgentContext,
    message: AssistantMessage,
    replace: bool,
) {
    if replace {
        if let Some(last) = context.messages.last_mut() {
            *last = Message::Assistant(message);
            return;
        }
    }
    context.messages.push(Message::Assistant(message));
}

fn tool_calls(message: &AssistantMessage) -> Vec<ToolCall> {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolCall(call) => Some(call.clone()),
            _ => None,
        })
        .collect()
}

async fn execute_tool_calls(
    context: &AgentContext,
    message: &AssistantMessage,
    config: &AgentLoopConfig,
    emit: EventSink,
    abort: AbortSignal,
) -> Result<ExecutedBatch> {
    let calls = tool_calls(message);
    let force_sequential = config.tool_execution == ToolExecutionMode::Sequential
        || calls.iter().any(|call| {
            context
                .tools
                .iter()
                .find(|tool| tool.name == call.name)
                .is_some_and(|tool| tool.execution_mode == ToolExecutionMode::Sequential)
        });
    if force_sequential {
        execute_tool_calls_sequential(context, message, config, emit, abort, calls).await
    } else {
        execute_tool_calls_parallel(context, message, config, emit, abort, calls).await
    }
}

async fn execute_tool_calls_sequential(
    context: &AgentContext,
    message: &AssistantMessage,
    config: &AgentLoopConfig,
    emit: EventSink,
    abort: AbortSignal,
    calls: Vec<ToolCall>,
) -> Result<ExecutedBatch> {
    let mut outcomes = Vec::with_capacity(calls.len());
    for call in calls {
        emit_tool_start(&call, emit.clone()).await?;
        let outcome = match prepare_tool_call(context, message, config, &call, abort.clone()).await
        {
            Ok(prepared) => {
                let executed =
                    execute_prepared_tool_call(prepared.clone(), emit.clone(), abort.clone(), None)
                        .await?;
                finalize_tool_call(context, message, config, prepared, executed).await
            }
            Err(error_result) => FinalizedOutcome {
                tool_call: call,
                result: error_result,
                is_error: true,
            },
        };
        emit_tool_end(&outcome, emit.clone()).await?;
        emit_tool_result(&outcome, emit.clone()).await?;
        outcomes.push(outcome);
        if abort.is_aborted() {
            break;
        }
    }
    Ok(outcomes_to_batch(outcomes))
}

async fn execute_tool_calls_parallel(
    context: &AgentContext,
    message: &AssistantMessage,
    config: &AgentLoopConfig,
    emit: EventSink,
    abort: AbortSignal,
    calls: Vec<ToolCall>,
) -> Result<ExecutedBatch> {
    enum Slot {
        Immediate(FinalizedOutcome),
        Prepared(PreparedToolCall),
    }
    let mut slots = Vec::with_capacity(calls.len());
    for call in calls {
        emit_tool_start(&call, emit.clone()).await?;
        match prepare_tool_call(context, message, config, &call, abort.clone()).await {
            Ok(prepared) => slots.push(Slot::Prepared(prepared)),
            Err(result) => {
                let outcome = FinalizedOutcome {
                    tool_call: call,
                    result,
                    is_error: true,
                };
                emit_tool_end(&outcome, emit.clone()).await?;
                slots.push(Slot::Immediate(outcome));
            }
        }
        if abort.is_aborted() {
            break;
        }
    }

    let finalization_lock = Arc::new(Mutex::new(()));
    let futures = slots.into_iter().map(|slot| {
        let emit = emit.clone();
        let abort = abort.clone();
        let context = context.clone();
        let message = message.clone();
        let config = config.clone();
        let finalization_lock = finalization_lock.clone();
        async move {
            match slot {
                Slot::Immediate(outcome) => Ok(outcome),
                Slot::Prepared(prepared) => {
                    let executed = execute_prepared_tool_call(
                        prepared.clone(),
                        emit.clone(),
                        abort,
                        Some(finalization_lock.clone()),
                    )
                    .await?;
                    let _guard = finalization_lock.lock().await;
                    let outcome =
                        finalize_tool_call(&context, &message, &config, prepared, executed).await;
                    emit_tool_end(&outcome, emit).await?;
                    Ok::<_, anyhow::Error>(outcome)
                }
            }
        }
    });
    let outcomes = join_all(futures)
        .await
        .into_iter()
        .collect::<Result<Vec<_>>>()?;

    // Completion/update events may arrive in execution order. Final result messages
    // are deliberately emitted only after the join, in the model's source order.
    for outcome in &outcomes {
        emit_tool_result(outcome, emit.clone()).await?;
    }
    Ok(outcomes_to_batch(outcomes))
}

async fn prepare_tool_call(
    context: &AgentContext,
    message: &AssistantMessage,
    config: &AgentLoopConfig,
    call: &ToolCall,
    abort: AbortSignal,
) -> std::result::Result<PreparedToolCall, AgentToolResult> {
    let Some(tool) = context
        .tools
        .iter()
        .find(|tool| tool.name == call.name)
        .cloned()
    else {
        return Err(error_tool_result(format!("Tool {} not found", call.name)));
    };

    let prepared = catch_unwind(AssertUnwindSafe(|| {
        tool.prepare_arguments.as_ref().map_or_else(
            || Ok(call.arguments.clone()),
            |prepare| prepare(call.arguments.clone()),
        )
    }))
    .map_err(panic_error)
    .and_then(|result| result)
    .map_err(|error| error_tool_result(error.to_string()))?;

    let validated = pi_ai::validate_tool_arguments(&tool.as_tool_definition(), &prepared)
        .map_err(|error| error_tool_result(error.to_string()))?;
    let mut arguments = validated;

    if let Some(before) = &config.before_tool_call {
        let before_context = BeforeToolCallContext {
            assistant_message: message.clone(),
            tool_call: call.clone(),
            arguments: arguments.clone(),
            context: context.clone(),
        };
        let outcome = AssertUnwindSafe(before(before_context))
            .catch_unwind()
            .await
            .map_err(|panic| error_tool_result(panic_message(panic)))?
            .map_err(|error| error_tool_result(error.to_string()))?;
        if abort.is_aborted() {
            return Err(error_tool_result("Operation aborted"));
        }
        if outcome.block {
            return Err(error_tool_result(
                outcome
                    .reason
                    .unwrap_or_else(|| "Tool execution was blocked".to_owned()),
            ));
        }
        if let Some(updated) = outcome.arguments {
            arguments = updated;
        }
    }
    if abort.is_aborted() {
        return Err(error_tool_result("Operation aborted"));
    }
    Ok(PreparedToolCall {
        tool_call: call.clone(),
        tool,
        arguments,
        model: config.model.clone(),
    })
}

async fn execute_prepared_tool_call(
    prepared: PreparedToolCall,
    emit: EventSink,
    abort: AbortSignal,
    finalization_lock: Option<Arc<Mutex<()>>>,
) -> Result<(AgentToolResult, bool)> {
    let accepting = Arc::new(AtomicBool::new(true));
    let (updates_tx, mut updates_rx) = mpsc::unbounded_channel::<AgentToolResult>();
    let (stop_tx, mut stop_rx) = oneshot::channel::<()>();
    let accepting_updates = accepting.clone();
    let update: ToolUpdateFn = Arc::new(move |partial| {
        if accepting_updates.load(Ordering::Acquire) {
            let _ = updates_tx.send(partial);
        }
    });
    let update_emit = emit.clone();
    let update_call = prepared.tool_call.clone();
    let update_task = tokio::spawn(async move {
        let mut first_error = None;
        loop {
            tokio::select! {
                Some(partial) = updates_rx.recv() => {
                    let event = AgentEvent::ToolExecutionUpdate {
                        tool_call_id: update_call.id.clone(),
                        tool_name: update_call.name.clone(),
                        arguments: update_call.arguments.clone(),
                        partial_result: partial,
                    };
                    // Serialize update events with finalization events across
                    // parallel tools: hold the shared lock for the duration of
                    // the emit so no other tool's update or end event interleaves.
                    // The lock is released when `_guard` drops at arm end, before
                    // the next select iteration, so it is never held across an
                    // await that re-acquires it (no deadlock).
                    let _guard = match &finalization_lock {
                        Some(lock) => Some(lock.lock().await),
                        None => None,
                    };
                    if let Err(error) = update_emit(event).await {
                        if first_error.is_none() {
                            first_error = Some(error);
                        }
                    }
                }
                _ = &mut stop_rx => {
                    while let Ok(partial) = updates_rx.try_recv() {
                        let event = AgentEvent::ToolExecutionUpdate {
                            tool_call_id: update_call.id.clone(),
                            tool_name: update_call.name.clone(),
                            arguments: update_call.arguments.clone(),
                            partial_result: partial,
                        };
                        let _guard = match &finalization_lock {
                            Some(lock) => Some(lock.lock().await),
                            None => None,
                        };
                        if let Err(error) = update_emit(event).await {
                            if first_error.is_none() {
                                first_error = Some(error);
                            }
                        }
                    }
                    return first_error;
                }
            }
        }
    });

    let tool_context = ToolCallContext {
        tool_call_id: prepared.tool_call.id.clone(),
        arguments: prepared.arguments.clone(),
        on_update: update,
        abort,
        model: Some(prepared.model.clone()),
    };
    let execution = AssertUnwindSafe((prepared.tool.execute)(tool_context))
        .catch_unwind()
        .await;
    accepting.store(false, Ordering::Release);
    let _ = stop_tx.send(());
    let update_error = update_task
        .await
        .map_err(|error| anyhow!("tool update task failed: {error}"))?;
    if let Some(error) = update_error {
        return Err(error);
    }

    Ok(match execution {
        Ok(Ok(result)) => (result, false),
        Ok(Err(error)) => (error_tool_result(error.to_string()), true),
        Err(panic) => (error_tool_result(panic_message(panic)), true),
    })
}

async fn finalize_tool_call(
    context: &AgentContext,
    message: &AssistantMessage,
    config: &AgentLoopConfig,
    prepared: PreparedToolCall,
    (mut result, mut is_error): (AgentToolResult, bool),
) -> FinalizedOutcome {
    if let Some(after) = &config.after_tool_call {
        let after_context = AfterToolCallContext {
            assistant_message: message.clone(),
            tool_call: prepared.tool_call.clone(),
            arguments: prepared.arguments,
            result: result.clone(),
            is_error,
            context: context.clone(),
        };
        match AssertUnwindSafe(after(after_context)).catch_unwind().await {
            Ok(Ok(update)) => {
                if let Some(content) = update.content {
                    result.content = content;
                }
                if let Some(details) = update.details {
                    result.details = details;
                }
                if let Some(usage) = update.usage {
                    result.usage = Some(usage);
                }
                if let Some(terminate) = update.terminate {
                    result.terminate = terminate;
                }
                if let Some(error) = update.is_error {
                    is_error = error;
                }
            }
            Ok(Err(error)) => {
                result = error_tool_result(error.to_string());
                is_error = true;
            }
            Err(panic) => {
                result = error_tool_result(panic_message(panic));
                is_error = true;
            }
        }
    }
    FinalizedOutcome {
        tool_call: prepared.tool_call,
        result,
        is_error,
    }
}

async fn fail_truncated_tool_calls(calls: &[ToolCall], emit: EventSink) -> Result<ExecutedBatch> {
    let mut outcomes = Vec::with_capacity(calls.len());
    for call in calls {
        emit_tool_start(call, emit.clone()).await?;
        let outcome = FinalizedOutcome {
            tool_call: call.clone(),
            result: error_tool_result(format!(
                "Tool call \"{}\" was not executed: the response hit the output token limit, so its arguments may be truncated. Re-issue the tool call with complete arguments.",
                call.name
            )),
            is_error: true,
        };
        emit_tool_end(&outcome, emit.clone()).await?;
        emit_tool_result(&outcome, emit.clone()).await?;
        outcomes.push(outcome);
    }
    Ok(outcomes_to_batch(outcomes))
}

async fn emit_tool_start(call: &ToolCall, emit: EventSink) -> Result<()> {
    emit(AgentEvent::ToolExecutionStart {
        tool_call_id: call.id.clone(),
        tool_name: call.name.clone(),
        arguments: call.arguments.clone(),
    })
    .await
}

async fn emit_tool_end(outcome: &FinalizedOutcome, emit: EventSink) -> Result<()> {
    emit(AgentEvent::ToolExecutionEnd {
        tool_call_id: outcome.tool_call.id.clone(),
        tool_name: outcome.tool_call.name.clone(),
        result: outcome.result.clone(),
        is_error: outcome.is_error,
    })
    .await
}

async fn emit_tool_result(outcome: &FinalizedOutcome, emit: EventSink) -> Result<()> {
    let message = Message::ToolResult(tool_result_message(outcome));
    emit(AgentEvent::MessageStart {
        message: message.clone(),
    })
    .await?;
    emit(AgentEvent::MessageEnd { message }).await
}

fn tool_result_message(outcome: &FinalizedOutcome) -> ToolResultMessage {
    ToolResultMessage {
        tool_call_id: outcome.tool_call.id.clone(),
        tool_name: outcome.tool_call.name.clone(),
        content: outcome.result.content.clone(),
        usage: outcome.result.usage.clone(),
        details: Some(outcome.result.details.clone()),
        added_tool_names: outcome.result.added_tool_names.clone(),
        is_error: outcome.is_error,
        timestamp: now_millis(),
    }
}

fn outcomes_to_batch(outcomes: Vec<FinalizedOutcome>) -> ExecutedBatch {
    let terminate = !outcomes.is_empty() && outcomes.iter().all(|item| item.result.terminate);
    let messages = outcomes.iter().map(tool_result_message).collect();
    ExecutedBatch {
        messages,
        terminate,
    }
}

fn error_tool_result(message: impl Into<String>) -> AgentToolResult {
    AgentToolResult::text(message)
}

fn panic_error(panic: Box<dyn std::any::Any + Send>) -> anyhow::Error {
    anyhow!(panic_message(panic))
}

fn panic_message(panic: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = panic.downcast_ref::<String>() {
        message.clone()
    } else if let Some(message) = panic.downcast_ref::<&'static str>() {
        (*message).to_owned()
    } else {
        "panic in agent callback".to_owned()
    }
}

fn now_millis() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}
