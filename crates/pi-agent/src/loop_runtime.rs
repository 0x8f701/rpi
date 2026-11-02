use std::{
    collections::HashSet,
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
use serde_json::Value;

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
    approval_token: Option<crate::ToolCallApprovalToken>,
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

/// Restores drained-but-undelivered pending messages through the loop
/// config's restore hook: steering back to the steering queue head, follow-up
/// batches back to the follow-up queue head, each preserving FIFO order. A
/// config without a restore hook (manual construction in tests) keeps the old
/// drop-on-failure behavior; the production `Agent` always installs both.
async fn restore_pending(
    config: &AgentLoopConfig,
    follow_up: bool,
    messages: &[AgentMessage],
) {
    if messages.is_empty() {
        return;
    }
    let restore = if follow_up {
        config.restore_follow_up_messages.as_ref()
    } else {
        config.restore_steering_messages.as_ref()
    };
    if let Some(restore) = restore {
        restore(messages.to_vec()).await;
    }
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

    let mut pending_from_follow_up = false;
    loop {
        let mut has_more_tool_calls = true;
        while has_more_tool_calls || !pending.is_empty() {
            if first_turn {
                first_turn = false;
            } else if let Err(error) = emit(AgentEvent::TurnStart).await {
                // The pending batch was already drained from its queue; the
                // failed TurnStart must not silently consume it.
                restore_pending(&config, pending_from_follow_up, &pending).await;
                return Err(error);
            }

            // Deliver the drained pending messages transactionally: a message
            // is pushed into the context only after its MessageEnd emit (and
            // its listeners) succeeded. On any emit failure the undelivered
            // tail — the failing message included, unless the agent state
            // already committed it — returns to the head of its queue in the
            // original order, so a failed transform/listener never drops
            // acknowledged steering.
            let mut delivery_error = None;
            for index in 0..pending.len() {
                let message = pending[index].clone();
                let delivered = async {
                    emit(AgentEvent::MessageStart {
                        message: message.clone(),
                    })
                    .await?;
                    emit(AgentEvent::MessageEnd {
                        message: message.clone(),
                    })
                    .await?;
                    Ok::<(), anyhow::Error>(())
                }
                .await;
                match delivered {
                    Ok(()) => {
                        context.messages.push(message.clone());
                        new_messages.push(message);
                    }
                    Err(error) => {
                        restore_pending(&config, pending_from_follow_up, &pending[index..]).await;
                        delivery_error = Some(error);
                        break;
                    }
                }
            }
            pending.clear();
            if let Some(error) = delivery_error {
                return Err(error);
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
            pending_from_follow_up = false;
        }

        let follow_ups = if let Some(get) = &config.get_follow_up_messages {
            get().await
        } else {
            Vec::new()
        };
        if follow_ups.is_empty() {
            // The final-turn window: a message steered while the last turn was
            // in flight (e.g. IRC handed to an active orchestration bridge)
            // arrives during the follow-up check, after the turn-boundary
            // drain already returned empty. Drain steering once more and
            // process anything that landed so the run never settles with an
            pending = if let Some(get) = &config.get_steering_messages {
                get().await
            } else {
                Vec::new()
            };
            pending_from_follow_up = false;
            if pending.is_empty() {
                break;
            }
        } else {
            pending = follow_ups;
            pending_from_follow_up = true;
        }
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
        ThinkingLevel::Max => Some(pi_ai::ThinkingLevel::Max),
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
                let message = normalize_terminal_assistant_message(context, message, added_partial);
                replace_or_append_assistant(context, message.clone(), added_partial);
                emit_assistant_completion(&message, added_partial, emit.clone()).await?;
                return Ok(message);
            }
            AssistantMessageEvent::Error { error, .. } => {
                let error = normalize_terminal_assistant_message(context, error, added_partial);
                replace_or_append_assistant(context, error.clone(), added_partial);
                let _listener_error =
                    emit_assistant_completion(&error, added_partial, emit.clone())
                        .await
                        .err();
                return Ok(error);
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
    let message = normalize_terminal_assistant_message(context, &message, added_partial);
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
            Err(error_result) => {
                finalize_failed_tool_call(
                    context,
                    message,
                    config,
                    &call,
                    &call.arguments,
                    error_result,
                )
                .await
            }
        };
        emit_tool_end(&outcome, emit.clone()).await?;
        emit_tool_result(&outcome, emit.clone()).await?;
        let terminated = outcome.result.terminate;
        outcomes.push(outcome);
        if abort.is_aborted() || terminated {
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
                let outcome = finalize_failed_tool_call(
                    context,
                    message,
                    config,
                    &call,
                    &call.arguments,
                    result,
                )
                .await;
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
        .map_err(|error| tool_validation_error(&tool, &prepared, &error.to_string()))?;
    let mut arguments = validated;

    if let Some(before) = &config.before_tool_call {
        let before_context = BeforeToolCallContext {
            assistant_message: message.clone(),
            tool_call: call.clone(),
            arguments: arguments.clone(),
            context: context.clone(),
            approval_request: None,
        };
        let outcome = AssertUnwindSafe(before(before_context))
            .catch_unwind()
            .await
            .map_err(|panic| error_tool_result(panic_message(panic)))?
            .map_err(|error| error_tool_result(error.to_string()))?;
        if abort.is_aborted() {
            return Err(error_tool_result("Operation aborted"));
        }
        if outcome.approval_token.is_some() {
            return Err(error_tool_result(
                "general before-tool-call hooks cannot issue approval tokens",
            ));
        }
        if outcome.block {
            return Err(error_tool_result(
                outcome
                    .reason
                    .unwrap_or_else(|| "Tool execution was blocked".to_owned()),
            ));
        }
        if let Some(updated) = outcome.arguments {
            arguments = pi_ai::validate_tool_arguments(&tool.as_tool_definition(), &updated)
                .map_err(|error| tool_validation_error(&tool, &updated, &error.to_string()))?;
        }
    }
    let approval_request = match &tool.prepare_approval {
        Some(prepare) => AssertUnwindSafe(prepare(arguments.clone()))
            .catch_unwind()
            .await
            .map_err(|panic| error_tool_result(panic_message(panic)))?
            .map_err(|error| error_tool_result(error.to_string()))?,
        None => None,
    };
    let approval_token = match approval_request {
        Some(approval_request) => {
            let Some(approve) = &config.approve_tool_call else {
                return Err(error_tool_result("tool approval adapter is unavailable"));
            };
            let approval_context = BeforeToolCallContext {
                assistant_message: message.clone(),
                tool_call: call.clone(),
                arguments: arguments.clone(),
                context: context.clone(),
                approval_request: Some(approval_request),
            };
            let outcome = AssertUnwindSafe(approve(approval_context))
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
            if outcome.arguments.is_some() {
                return Err(error_tool_result(
                    "final approval hook cannot rewrite tool arguments",
                ));
            }
            Some(outcome.approval_token.ok_or_else(|| {
                error_tool_result("final approval hook did not issue an approval token")
            })?)
        }
        None => None,
    };
    if abort.is_aborted() {
        return Err(error_tool_result("Operation aborted"));
    }
    Ok(PreparedToolCall {
        tool_call: call.clone(),
        tool,
        arguments,
        model: config.model.clone(),
        approval_token,
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
        approval_token: prepared.approval_token,
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

async fn apply_after_tool_call(
    context: &AgentContext,
    message: &AssistantMessage,
    config: &AgentLoopConfig,
    tool_call: ToolCall,
    arguments: serde_json::Value,
    mut result: AgentToolResult,
    mut is_error: bool,
) -> FinalizedOutcome {
    if let Some(after) = &config.after_tool_call {
        let after_context = AfterToolCallContext {
            assistant_message: message.clone(),
            tool_call: tool_call.clone(),
            arguments,
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
        tool_call,
        result,
        is_error,
    }
}

async fn finalize_tool_call(
    context: &AgentContext,
    message: &AssistantMessage,
    config: &AgentLoopConfig,
    prepared: PreparedToolCall,
    executed: (AgentToolResult, bool),
) -> FinalizedOutcome {
    let (result, is_error) = executed;
    apply_after_tool_call(
        context,
        message,
        config,
        prepared.tool_call,
        prepared.arguments,
        result,
        is_error,
    )
    .await
}

/// Finalizes a tool call that failed before execution (schema validation or a
/// prepare/before/approval hook). The after-tool-call hook sees the original
/// proposed arguments and `is_error = true` exactly once, and may replace the
/// failure content or set `terminate` so the session loop can stop repeated
/// invalid calls. The call itself never flips to a success: the tool did not
/// run, so execution and validation errors stay distinct.
async fn finalize_failed_tool_call(
    context: &AgentContext,
    message: &AssistantMessage,
    config: &AgentLoopConfig,
    call: &ToolCall,
    arguments: &serde_json::Value,
    result: AgentToolResult,
) -> FinalizedOutcome {
    let mut outcome = apply_after_tool_call(
        context,
        message,
        config,
        call.clone(),
        arguments.clone(),
        result,
        true,
    )
    .await;
    outcome.is_error = true;
    outcome
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
    // Terminate is a batch stop request: any sibling outcome asking to stop
    // prevents the next model turn, while every already-started sibling still
    let terminate = outcomes.iter().any(|item| item.result.terminate);
    let messages = outcomes.iter().map(tool_result_message).collect();
    ExecutedBatch {
        messages,
        terminate,
    }
}

fn error_tool_result(message: impl Into<String>) -> AgentToolResult {
    AgentToolResult::text(message)
}
/// A compact corrected example for [`tool_validation_error`]: candidates are
/// tried in order — const, default, enum members, anyOf/oneOf branches, then
/// type-derived candidates honoring numeric/string/array/object constraints —
/// and only a candidate that the complete schema validator accepts is
/// returned. When no compact valid candidate can be synthesized (arbitrary
/// patterns, allOf combinations), the caller omits the example instead of
/// emitting an invalid one. Never echoes user-supplied argument values.
const SCHEMA_EXAMPLE_MAX_DEPTH: usize = 16;

fn schema_example(schema: &pi_ai::Schema) -> Option<serde_json::Value> {
    schema_example_at(schema, SCHEMA_EXAMPLE_MAX_DEPTH)
}

fn schema_example_at(schema: &pi_ai::Schema, depth: usize) -> Option<serde_json::Value> {
    if depth == 0 {
        return None;
    }
    if let Some(const_value) = &schema.const_value {
        if schema.validate(const_value).is_ok() {
            return Some(const_value.clone());
        }
    }
    if let Some(default) = &schema.default {
        if schema.validate(default).is_ok() {
            return Some(default.clone());
        }
    }
    for member in &schema.enum_values {
        if schema.validate(member).is_ok() {
            return Some(member.clone());
        }
    }
    for branch in schema.any_of.iter().chain(&schema.one_of) {
        if let Some(candidate) = schema_example_at(branch, depth - 1) {
            if schema.validate(&candidate).is_ok() {
                return Some(candidate);
            }
        }
    }
    let mut kinds = schema_type_kinds(schema);
    if kinds.is_empty() && !schema.properties.is_empty() {
        kinds.push("object");
    }
    for kind in kinds {
        let candidate = match kind {
            "object" => object_example(schema, depth - 1),
            "array" => array_example(schema, depth - 1),
            "string" => string_example(schema),
            "integer" => number_example(schema, true),
            "number" => number_example(schema, false),
            "boolean" => Some(serde_json::Value::Bool(false)),
            "null" => Some(serde_json::Value::Null),
            _ => None,
        };
        if let Some(candidate) = candidate {
            if schema.validate(&candidate).is_ok() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Declared primary kinds of a schema, mirroring the validator's own type
/// resolution (primary type or type array, `nullable` widening to null).
fn schema_type_kinds(schema: &pi_ai::Schema) -> Vec<&str> {
    let mut kinds: Vec<&str> = match &schema.schema_type {
        Some(Value::String(kind)) => vec![kind.as_str()],
        Some(Value::Array(kinds)) => kinds.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    if schema.nullable && !kinds.contains(&"null") {
        kinds.push("null");
    }
    kinds
}

fn object_example(schema: &pi_ai::Schema, depth: usize) -> Option<serde_json::Value> {
    // Complete skeleton first; fall back to required-only so a hostile or
    // unsatisfiable optional cannot push the whole example out of validity.
    if let Some(candidate) = object_skeleton(schema, depth, false) {
        if schema.validate(&candidate).is_ok() {
            return Some(candidate);
        }
    }
    let required_only = object_skeleton(schema, depth, true)?;
    if schema.validate(&required_only).is_ok() {
        Some(required_only)
    } else {
        None
    }
}

/// Property names the schema declares, in `property_order` order with a
/// sorted fallback; a partially populated `property_order` never hides a
/// required field.
fn declared_property_names(schema: &pi_ai::Schema) -> Vec<&str> {
    let mut names: Vec<&str> = schema
        .property_order
        .iter()
        .filter(|name| schema.properties.contains_key(name.as_str()))
        .map(String::as_str)
        .collect();
    if names.is_empty() {
        names = schema.properties.keys().map(String::as_str).collect();
        names.sort_unstable();
    }
    for name in &schema.required {
        if !names.contains(&name.as_str()) && schema.properties.contains_key(name.as_str()) {
            names.push(name.as_str());
        }
    }
    names
}

fn object_skeleton(
    schema: &pi_ai::Schema,
    depth: usize,
    required_only: bool,
) -> Option<serde_json::Value> {
    // Required fields come first so no ordering quirk can drop them.
    let mut ordered: Vec<&str> = schema.required.iter().map(String::as_str).collect();
    if !required_only {
        let mut optionals = declared_property_names(schema);
        optionals.retain(|name| !ordered.contains(name));
        ordered.append(&mut optionals);
    }
    let mut skeleton = serde_json::Map::new();
    for name in ordered {
        let field_schema = schema.properties.get(name)?;
        if let Some(value) = schema_example_at(field_schema, depth) {
            skeleton.insert(name.to_owned(), value);
        } else if schema.required.iter().any(|required| required == name) {
            // A required field with no valid candidate: no skeleton can
            // satisfy the schema, so the example is omitted entirely.
            return None;
        }
    }
    Some(serde_json::Value::Object(skeleton))
}

fn array_example(schema: &pi_ai::Schema, depth: usize) -> Option<serde_json::Value> {
    let min = schema.min_items.unwrap_or(0);
    if let Some(max) = schema.max_items {
        if min > max {
            return None;
        }
    }
    let item_schema = schema.items.as_deref()?;
    let mut items = Vec::with_capacity(min);
    for _ in 0..min {
        items.push(schema_example_at(item_schema, depth)?);
    }
    Some(serde_json::Value::Array(items))
}

fn string_example(schema: &pi_ai::Schema) -> Option<serde_json::Value> {
    if schema.pattern.is_some() {
        // Arbitrary patterns cannot be inverted safely; omit rather than
        // guessing a value that is unlikely to match.
        return None;
    }
    let min = schema.min_length.unwrap_or(0);
    if let Some(max) = schema.max_length {
        if min > max {
            return None;
        }
    }
    Some(serde_json::Value::String("x".repeat(min)))
}

fn number_example(schema: &pi_ai::Schema, integer: bool) -> Option<serde_json::Value> {
    let mut values: Vec<f64> = vec![0.0, 1.0, -1.0];
    if let Some(bound) = schema.minimum {
        values.push(bound);
        values.push(bound + 1.0);
    }
    if let Some(bound) = schema.exclusive_minimum {
        values.push(bound + 0.5);
        values.push(bound + 1.0);
    }
    if let Some(bound) = schema.maximum {
        values.push(bound);
        values.push(bound - 1.0);
    }
    if let Some(bound) = schema.exclusive_maximum {
        values.push(bound - 0.5);
        values.push(bound - 1.0);
    }
    if let Some(multiple) = schema.multiple_of {
        if multiple > 0.0 {
            values.push(multiple);
            values.push(multiple * 2.0);
        }
    }
    for value in values {
        if integer && value.fract() != 0.0 {
            continue;
        }
        let candidate = if value.fract() == 0.0 && value.abs() <= i64::MAX as f64 {
            serde_json::Value::from(value as i64)
        } else {
            serde_json::Value::from(value)
        };
        if schema.validate(&candidate).is_ok() {
            return Some(candidate);
        }
    }
    None
}

/// Only schema-declared fields and JSON type names supplement the redacted
/// category; argument values and raw validator paths must never be surfaced.
fn tool_validation_error(
    tool: &AgentTool,
    arguments: &serde_json::Value,
    reason: &str,
) -> AgentToolResult {
    let schema = &tool.parameters;
    let allowed = declared_property_names(schema);
    let mut message = format!(
        "Invalid {} arguments: {}. Allowed fields: {}.",
        tool.name,
        stable_validation_reason(reason),
        allowed.join(", "),
    );
    if let Some(diagnostic) = declared_field_diagnostic(schema, arguments, &allowed) {
        message.push(' ');
        message.push_str(&diagnostic);
    }
    if let Some(example) = schema_example(schema) {
        message.push_str(&format!(
            " Corrected example: {}",
            serde_json::to_string(&example).unwrap_or_else(|_| "{}".to_owned())
        ));
    }
    error_tool_result(message)
}

fn declared_field_diagnostic(
    schema: &pi_ai::Schema,
    arguments: &serde_json::Value,
    allowed: &[&str],
) -> Option<String> {
    let arguments = arguments.as_object()?;
    for &name in allowed {
        if name.is_empty() || name.len() > 64
            || !name.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        {
            continue;
        }
        let Some(property) = schema.properties.get(name) else { continue };
        let Some(value) = arguments.get(name) else { continue };
        if property.validate(value).is_ok() {
            continue;
        }
        let received = match value {
            serde_json::Value::Null => "null",
            serde_json::Value::Bool(_) => "boolean",
            serde_json::Value::Number(_) => "number",
            serde_json::Value::String(_) => "string",
            serde_json::Value::Array(_) => "array",
            serde_json::Value::Object(_) => "object",
        };
        let expected = match property.schema_type.as_ref().and_then(serde_json::Value::as_str) {
            Some(kind @ ("null" | "boolean" | "number" | "integer" | "string" | "array" | "object"))
                if !property.nullable && kind != received
                    && !(kind == "integer"
                        && value.as_f64().is_some_and(|number| number.fract() == 0.0)) => Some(kind),
            _ => None,
        };
        return Some(match expected {
            Some(expected) => format!("Field {name}: expected {expected}, received {received}."),
            None => format!("Field {name}: received {received}; value does not match declared schema."),
        });
    }
    None
}

/// Reduces a validator message to its stable category. The raw validator text
/// embeds the JSON path (`$.<name>: category`) where `<name>` is
/// user-controlled, so only the category after the last `: ` separator is
/// echoed — and only when the prefix is a validator path (leading `$`).
/// Anything else falls back to a fixed phrase. Keys, paths, and values are
/// never surfaced.
fn stable_validation_reason(reason: &str) -> &str {
    let trimmed = reason.trim().trim_end_matches('.');
    match trimmed.rsplit_once(": ") {
        Some((path, category)) if path.starts_with('$') && !category.trim().is_empty() => category,
        _ => "arguments failed schema validation",
    }
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

const SYNTHESIZED_TOOL_CALL_ID_LEN: usize = 9;

/// Normalize tool-call IDs on a terminal assistant message before it is stored,
/// emitted as completion/MessageEnd, used for tool execution, or returned.
///
/// Empty IDs and duplicates (within the message or against prior assistant
/// messages in context) are replaced with deterministic 9-char alphanumeric
/// IDs (`p` + 8 base36 chars). Unique nonempty provider IDs are preserved.
/// Partial streaming updates may remain raw.
fn normalize_terminal_assistant_message(
    context: &AgentContext,
    message: &AssistantMessage,
    exclude_trailing_partial: bool,
) -> AssistantMessage {
    let prior_messages = prior_context_messages(&context.messages, exclude_trailing_partial);
    let mut used = collect_prior_tool_call_ids(prior_messages);
    // Ordinal/length exclude the in-flight partial so synthesis stays stable
    // whether Start already pushed a placeholder assistant or not.
    let assistant_ordinal = prior_messages
        .iter()
        .filter(|message| matches!(message, Message::Assistant(_)))
        .count();
    let context_len = prior_messages.len();
    let mut normalized = message.clone();

    for (content_index, block) in normalized.content.iter_mut().enumerate() {
        let ContentBlock::ToolCall(call) = block else {
            continue;
        };
        let original_id = call.id.clone();
        if !original_id.is_empty() && used.insert(original_id.clone()) {
            continue;
        }
        let mut probe = 0u64;
        let synthesized = loop {
            let candidate = synthesize_tool_call_id(
                assistant_ordinal,
                context_len,
                message.timestamp,
                content_index,
                &call.name,
                &original_id,
                probe,
            );
            if used.insert(candidate.clone()) {
                break candidate;
            }
            probe = probe.saturating_add(1);
        };
        call.id = synthesized;
    }

    normalized
}

fn prior_context_messages(
    messages: &[AgentMessage],
    exclude_trailing_partial: bool,
) -> &[AgentMessage] {
    if exclude_trailing_partial && !messages.is_empty() {
        &messages[..messages.len() - 1]
    } else {
        messages
    }
}

fn collect_prior_tool_call_ids(messages: &[AgentMessage]) -> HashSet<String> {
    let mut used = HashSet::new();
    for message in messages {
        match message {
            Message::Assistant(assistant) => {
                for block in &assistant.content {
                    if let ContentBlock::ToolCall(call) = block {
                        if !call.id.is_empty() {
                            used.insert(call.id.clone());
                        }
                    }
                }
            }
            Message::ToolResult(result) => {
                if !result.tool_call_id.is_empty() {
                    used.insert(result.tool_call_id.clone());
                }
            }
            _ => {}
        }
    }
    used
}

fn synthesize_tool_call_id(
    assistant_ordinal: usize,
    context_len: usize,
    timestamp: i64,
    content_index: usize,
    tool_name: &str,
    original_id: &str,
    probe: u64,
) -> String {
    let mut hasher = Fnv1a64::new();
    hasher.write_u64(assistant_ordinal as u64);
    hasher.write_u64(context_len as u64);
    hasher.write_i64(timestamp);
    hasher.write_u64(content_index as u64);
    hasher.write_str(tool_name);
    hasher.write_str(original_id);
    hasher.write_u64(probe);
    let digest = hasher.finish();
    let mut id = String::with_capacity(SYNTHESIZED_TOOL_CALL_ID_LEN);
    id.push('p');
    append_base36_fixed(&mut id, digest);
    debug_assert_eq!(id.len(), SYNTHESIZED_TOOL_CALL_ID_LEN);
    debug_assert!(id.chars().all(|ch| ch.is_ascii_alphanumeric()));
    id
}

fn append_base36_fixed(out: &mut String, mut value: u64) {
    // Exactly 8 base36 digits → `p` + 8 = 9-char Mistral-safe IDs.
    let mut digits = [b'0'; 8];
    for slot in digits.iter_mut().rev() {
        let digit = (value % 36) as u8;
        *slot = if digit < 10 {
            b'0' + digit
        } else {
            b'a' + digit - 10
        };
        value /= 36;
    }
    out.push_str(std::str::from_utf8(&digits).expect("base36 digits are ASCII"));
}

/// Fixed FNV-1a 64-bit hasher for cross-process / cross-Rust-version stability.
struct Fnv1a64 {
    state: u64,
}

impl Fnv1a64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x100_0000_01b3;

    fn new() -> Self {
        Self {
            state: Self::OFFSET,
        }
    }

    fn write_bytes(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.state ^= u64::from(*byte);
            self.state = self.state.wrapping_mul(Self::PRIME);
        }
    }

    fn write_str(&mut self, value: &str) {
        self.write_u64(value.len() as u64);
        self.write_bytes(value.as_bytes());
    }

    fn write_u64(&mut self, value: u64) {
        self.write_bytes(&value.to_le_bytes());
    }

    fn write_i64(&mut self, value: i64) {
        self.write_u64(value as u64);
    }

    fn finish(self) -> u64 {
        self.state
    }
}
fn now_millis() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod approval_tests {
    use parking_lot::Mutex as StdMutex;
    use pi_ai::{Model, Schema};
    use serde_json::json;

    use super::*;
    use crate::{
        AbortController, BeforeToolCallResult, ToolApprovalDigest, ToolApprovalId,
        ToolApprovalRequest, ToolApprovalSummary, ToolCallApprovalToken,
    };

    fn token() -> ToolCallApprovalToken {
        let mut bytes = [0_u8; 16];
        bytes[6] = 0x70;
        bytes[8] = 0x80;
        ToolCallApprovalToken::new(
            ToolApprovalId::from_uuid_v7_bytes(bytes).expect("uuid v7"),
            ToolApprovalDigest::from_sha256([7_u8; 32]),
        )
    }

    #[tokio::test]
    async fn approval_runs_after_final_rewrite_and_token_reaches_context() {
        let observed = Arc::new(StdMutex::new(None));
        let execution_observed = observed.clone();
        let expected = token();
        let tool = AgentTool::new("spawn", "spawn", Schema::default(), move |context| {
            let observed = execution_observed.clone();
            async move {
                *observed.lock() = Some((context.arguments, context.approval_token));
                Ok(AgentToolResult::text("ok"))
            }
        })
        .with_prepare_approval(move |arguments| {
            assert_eq!(arguments, json!({"value": "final"}));
            Box::pin(async move {
                Ok(Some(ToolApprovalRequest::new(
                    ToolApprovalSummary {
                        family: "managed-pipe-process".to_owned(),
                        executable: "tool".to_owned(),
                        argument_count: 1,
                        working_directory: "/workspace".to_owned(),
                        credential_audience: None,
                        requested_credential_count: 0,
                        target_fds: Vec::new(),
                    },
                    Box::new(move || Box::pin(async move { Ok(expected) })),
                )))
            })
        });
        let context = AgentContext {
            system_prompt: String::new(),
            messages: Vec::new(),
            tools: vec![tool],
        };
        let model = Model::default();
        let message = AssistantMessage::pending(&model);
        let call = ToolCall {
            id: "call".to_owned(),
            name: "spawn".to_owned(),
            arguments: json!({"value": "initial"}),
            thought_signature: None,
        };
        let config = AgentLoopConfig {
            model,
            before_tool_call: Some(Arc::new(|_| {
                Box::pin(async {
                    Ok(BeforeToolCallResult {
                        arguments: Some(json!({"value": "final"})),
                        ..BeforeToolCallResult::default()
                    })
                })
            })),
            approve_tool_call: Some(Arc::new(|context| {
                Box::pin(async move {
                    let approval_token = context
                        .approval_request
                        .expect("approval request")
                        .issue()
                        .await?;
                    Ok(BeforeToolCallResult {
                        approval_token: Some(approval_token),
                        ..BeforeToolCallResult::default()
                    })
                })
            })),
            ..AgentLoopConfig::default()
        };
        let (_, abort) = AbortController::new();
        let prepared = prepare_tool_call(&context, &message, &config, &call, abort.clone())
            .await
            .expect("prepare");
        let emit: EventSink = Arc::new(|_| Box::pin(async { Ok(()) }));
        execute_prepared_tool_call(prepared, emit, abort, None)
            .await
            .expect("execute");
        assert_eq!(
            *observed.lock(),
            Some((json!({"value": "final"}), Some(expected)))
        );
    }

    #[tokio::test]
    async fn approval_request_issues_at_most_once() {
        let request = ToolApprovalRequest::new(
            ToolApprovalSummary {
                family: "hook".to_owned(),
                executable: "hook".to_owned(),
                argument_count: 0,
                working_directory: "/workspace".to_owned(),
                credential_audience: None,
                requested_credential_count: 0,
                target_fds: Vec::new(),
            },
            Box::new(|| Box::pin(async { Ok(token()) })),
        );
        let clone = request.clone();
        assert_eq!(request.issue().await.expect("first"), token());
        let error = clone.issue().await.expect_err("second issue must fail");
        assert_eq!(error.to_string(), "tool approval request already issued");
    }
}
#[cfg(test)]
mod validation_error_tests {
    use std::{
        collections::VecDeque,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use parking_lot::Mutex as StdMutex;
    use pi_ai::{
        Model, Schema, SimpleStreamOptions, new_assistant_message_event_stream,
    };
    use serde_json::{Value, json};

    use super::*;
    use crate::{AbortController, AfterToolCallFn, AfterToolCallResult};

    /// Strict grep-like schema: required `pattern`, current optional fields,
    /// no additional properties — any OMP-era `case`/`gitignore`/`skip` extras
    /// must be rejected against the schema's actual current fields.
    fn strict_grep_schema() -> Schema {
        let mut schema = Schema::object_ordered(vec![
            ("pattern".to_owned(), Schema::string(), true),
            ("path".to_owned(), Schema::string(), false),
            ("glob".to_owned(), Schema::string(), false),
            (
                "ignoreCase".to_owned(),
                Schema {
                    schema_type: Some(Value::String("boolean".into())),
                    ..Schema::default()
                },
                false,
            ),
            (
                "literal".to_owned(),
                Schema {
                    schema_type: Some(Value::String("boolean".into())),
                    ..Schema::default()
                },
                false,
            ),
            (
                "context".to_owned(),
                Schema {
                    schema_type: Some(Value::String("number".into())),
                    ..Schema::default()
                },
                false,
            ),
            (
                "limit".to_owned(),
                Schema {
                    schema_type: Some(Value::String("number".into())),
                    ..Schema::default()
                },
                false,
            ),
        ]);
        schema.additional_properties = Some(Value::Bool(false));
        schema
    }

    fn grep_tool() -> AgentTool {
        AgentTool::new("grep", "search", strict_grep_schema(), |_| async {
            Ok(AgentToolResult::text("ok"))
        })
    }

    fn grep_call(id: &str, arguments: Value) -> ToolCall {
        ToolCall {
            id: id.to_owned(),
            name: "grep".to_owned(),
            arguments,
            thought_signature: None,
        }
    }

    fn result_text_from_blocks(content: &[ContentBlock]) -> String {
        content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn result_text(result: &AgentToolResult) -> String {
        result_text_from_blocks(&result.content)
    }

    fn stop_assistant() -> AssistantMessage {
        AssistantMessage {
            content: vec![ContentBlock::text("done")],
            stop_reason: StopReason::Stop,
            ..AssistantMessage::pending(&Model::default())
        }
    }

    fn scripted_assistant(messages: Vec<AssistantMessage>) -> StreamFn {
        let queue = Arc::new(StdMutex::new(VecDeque::from(messages)));
        Arc::new(
            move |_model: Model, _context: Context, _options: SimpleStreamOptions| {
                let queue = queue.clone();
                Box::pin(async move {
                    let message = queue.lock().pop_front().unwrap_or_else(stop_assistant);
                    let stream = new_assistant_message_event_stream();
                    let producer = stream.clone();
                    let terminal = if matches!(
                        message.stop_reason,
                        StopReason::Error | StopReason::Aborted
                    ) {
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
                    tokio::spawn(async move {
                        producer.push(terminal).await;
                        producer.end(Some(message)).await;
                    });
                    stream
                })
            },
        )
    }

    async fn prepare_rejecting(tool: &AgentTool, arguments: Value) -> AgentToolResult {
        let context = AgentContext {
            system_prompt: String::new(),
            messages: Vec::new(),
            tools: vec![tool.clone()],
        };
        let config = AgentLoopConfig {
            model: Model::default(),
            ..AgentLoopConfig::default()
        };
        let message = AssistantMessage::pending(&Model::default());
        let (_, abort) = AbortController::new();
        let call = call_for(tool, "call", arguments);
        match prepare_tool_call(&context, &message, &config, &call, abort).await {
            Err(result) => result,
            Ok(_) => panic!("must fail schema validation"),
        }
    }

    fn call_for(tool: &AgentTool, id: &str, arguments: Value) -> ToolCall {
        ToolCall {
            id: id.to_owned(),
            name: tool.name.clone(),
            arguments,
            thought_signature: None,
        }
    }

    fn tool_with_schema(name: &str, schema: Schema) -> AgentTool {
        AgentTool::new(name, "test tool", schema, |_| async {
            Ok(AgentToolResult::text("ok"))
        })
    }

    #[test]
    fn declared_field_diagnostic_is_bounded_and_ignores_unsafe_names() {
        let long_name = "x".repeat(65);
        let schema = Schema::object_ordered(vec![
            (long_name.clone(), Schema::string(), false),
            ("input.private-key".into(), Schema::string(), false),
            ("input".into(), Schema::string(), false),
            ("command".into(), Schema::string(), false),
        ]);
        let arguments = json!({
            (long_name): false,
            "input.private-key": false,
            "input": false,
            "command": false,
        });
        assert_eq!(
            declared_field_diagnostic(&schema, &arguments, &declared_property_names(&schema)),
            Some("Field input: expected string, received boolean.".into()),
        );
    }

    #[tokio::test]
    async fn nested_array_failure_reports_schema_mismatch_without_values() {
        let tool = tool_with_schema("test", Schema::object_ordered(vec![
            ("input".into(), Schema {
                schema_type: Some(json!("array")),
                items: Some(Box::new(Schema::string())),
                ..Schema::default()
            }, false),
        ]));
        let error = prepare_rejecting(&tool, json!({
            "input": [{"private-nested-key": "private-nested-value"}],
        })).await;
        let text = result_text(&error);
        assert!(text.contains("Field input: received array; value does not match declared schema."), "{text}");
        assert!(!text.contains("expected array, received array"), "{text}");
        assert!(!text.contains("private-"), "{text}");
    }

    #[test]
    fn matching_type_constraints_and_unions_report_schema_mismatch() {
        for (property, value, received) in [
            (Schema {
                schema_type: Some(json!("integer")),
                minimum: Some(2.0),
                ..Schema::default()
            }, json!(1), "number"),
            (Schema {
                any_of: vec![Schema::string(), Schema {
                    const_value: Some(json!(false)),
                    ..Schema::default()
                }],
                ..Schema::default()
            }, json!(true), "boolean"),
        ] {
            let schema = Schema::object_ordered(vec![("input".into(), property, false)]);
            assert_eq!(
                declared_field_diagnostic(&schema, &json!({"input": value}), &["input"]),
                Some(format!("Field input: received {received}; value does not match declared schema.")),
            );
        }
    }

    #[tokio::test]
    async fn invalid_declared_input_reports_field_and_types_without_values() {
        let tool = tool_with_schema("bash", Schema::object_ordered(vec![
            ("command".into(), Schema::string(), true),
            ("input".into(), Schema::string(), false),
        ]));
        for input in [json!(false), json!(true)] {
            let error = prepare_rejecting(
                &tool,
                json!({"command": "private-command-value", "input": input}),
            ).await;
            let text = result_text(&error);
            assert!(text.contains("Field input: expected string, received boolean."), "{text}");
            assert!(!text.contains("private-command-value"), "{text}");
            assert!(!text.contains("true"), "{text}");
            assert!(!text.contains("false"), "{text}");
        }
    }

    #[tokio::test]
    async fn invalid_declared_field_does_not_echo_nested_keys_or_values() {
        let tool = tool_with_schema("bash", Schema::object_ordered(vec![
            ("command".into(), Schema::string(), true),
            ("input".into(), Schema::string(), false),
        ]));
        let error = prepare_rejecting(&tool, json!({
            "command": "private-command-value",
            "input": {"private-nested-key": "private-nested-value"},
        })).await;
        let text = result_text(&error);
        assert!(text.contains("Field input: expected string, received object."), "{text}");
        assert!(!text.contains("private-"), "{text}");
        assert!(!text.contains("$."), "{text}");
    }

    #[tokio::test]
    async fn unknown_path_cannot_impersonate_a_declared_field() {
        let mut schema = Schema::object_ordered(vec![
            ("input".into(), Schema::string(), false),
        ]);
        schema.additional_properties = Some(json!({"type": "string"}));
        let tool = tool_with_schema("bash", schema);
        let error = prepare_rejecting(&tool, json!({
            "input": "private-input-value",
            "private-key: $.input": false,
        })).await;
        let text = result_text(&error);
        assert!(!text.contains("Field input:"), "{text}");
        assert!(!text.contains("private-"), "{text}");
        assert!(!text.contains("$."), "{text}");
    }

    #[tokio::test]
    async fn before_hook_validation_reports_updated_field_type() {
        let tool = tool_with_schema("bash", Schema::object_ordered(vec![
            ("input".into(), Schema::string(), false),
        ]));
        let context = AgentContext {
            tools: vec![tool.clone()],
            system_prompt: String::new(),
            messages: Vec::new(),
        };
        let config = AgentLoopConfig {
            model: Model::default(),
            before_tool_call: Some(Arc::new(|_| Box::pin(async {
                Ok(crate::BeforeToolCallResult {
                    arguments: Some(json!({"input": true})),
                    ..crate::BeforeToolCallResult::default()
                })
            }))),
            ..AgentLoopConfig::default()
        };
        let (_, abort) = AbortController::new();
        let error = match prepare_tool_call(
            &context,
            &AssistantMessage::pending(&Model::default()),
            &config,
            &call_for(&tool, "updated", json!({"input": "private-input-value"})),
            abort,
        ).await {
            Err(error) => error,
            Ok(_) => panic!("invalid hook arguments must fail"),
        };
        let text = result_text(&error);
        assert!(text.contains("Field input: expected string, received boolean."), "{text}");
        assert!(!text.contains("private-input-value"), "{text}");
    }

    #[tokio::test]
    async fn stale_extras_report_current_fields_with_valid_corrected_example() {
        let tool = grep_tool();
        let error = prepare_rejecting(
            &tool,
            json!({"pattern": "needle", "case": true, "gitignore": true, "skip": 3}),
        )
        .await;
        let text = result_text(&error);
        assert!(text.starts_with("Invalid grep arguments: "), "{text}");
        assert!(text.contains("additional property not allowed"), "{text}");
        assert!(
            text.contains("Allowed fields: pattern, path, glob, ignoreCase, literal, context, limit"),
            "{text}"
        );
        assert!(!text.contains("needle"), "must not leak argument values: {text}");
        assert!(
            !text.contains("gitignore"),
            "stale extras must not appear as allowed fields: {text}"
        );
        assert!(
            !text.contains("skip"),
            "stale extras must not appear as allowed fields: {text}"
        );
        let example = text
            .split("Corrected example: ")
            .nth(1)
            .expect("corrected example present");
        let skeleton: Value = serde_json::from_str(example).expect("skeleton is JSON");
        pi_ai::validate_tool_arguments(&tool.as_tool_definition(), &skeleton)
            .expect("corrected skeleton passes the strict schema");
        assert_eq!(skeleton["pattern"], json!(""));
        assert_eq!(skeleton["path"], json!(""));
        assert_eq!(skeleton["glob"], json!(""));
        assert_eq!(skeleton["ignoreCase"], json!(false));
        assert_eq!(skeleton["literal"], json!(false));
        assert_eq!(skeleton["context"], json!(0));
        assert_eq!(skeleton["limit"], json!(0));
        assert_eq!(skeleton.get("case"), None);
        assert_eq!(skeleton.get("gitignore"), None);

        // A valid call still prepares normally.
        let context = AgentContext {
            system_prompt: String::new(),
            messages: Vec::new(),
            tools: vec![tool.clone()],
        };
        let config = AgentLoopConfig {
            model: Model::default(),
            ..AgentLoopConfig::default()
        };
        let message = AssistantMessage::pending(&Model::default());
        let (_, abort) = AbortController::new();
        let prepared = prepare_tool_call(
            &context,
            &message,
            &config,
            &grep_call("valid", json!({"pattern": "needle"})),
            abort,
        )
        .await
        .expect("valid call prepares");
        assert_eq!(prepared.arguments, json!({"pattern": "needle"}));
    }

    #[tokio::test]
    async fn required_missing_lists_current_fields() {
        let tool = grep_tool();
        let error = prepare_rejecting(&tool, json!({})).await;
        let text = result_text(&error);
        assert!(text.starts_with("Invalid grep arguments: "), "{text}");
        assert!(text.contains("required property missing"), "{text}");
        assert!(
            text.contains("Allowed fields: pattern, path, glob, ignoreCase, literal, context, limit"),
            "{text}"
        );
        assert!(text.contains("Corrected example: "), "{text}");
    }

    #[tokio::test]
    async fn after_hook_sees_schema_failure_and_terminates_repeat_invalid_call() {
        let executed = Arc::new(AtomicUsize::new(0));
        let tool_executed = executed.clone();
        let tool = AgentTool::new("grep", "search", strict_grep_schema(), move |_| {
            let executed = tool_executed.clone();
            async move {
                executed.fetch_add(1, Ordering::SeqCst);
                Ok(AgentToolResult::text("ok"))
            }
        });
        let invalid_turn = AssistantMessage {
            content: vec![ContentBlock::ToolCall(grep_call(
                "repeat",
                json!({"pattern": "needle", "case": true}),
            ))],
            stop_reason: StopReason::ToolUse,
            ..AssistantMessage::pending(&Model::default())
        };
        let stream_calls = Arc::new(AtomicUsize::new(0));
        let calls = stream_calls.clone();
        let scripted = scripted_assistant(vec![
            invalid_turn.clone(),
            invalid_turn,
            stop_assistant(),
        ]);
        let stream: StreamFn = Arc::new(
            move |model: Model, context: Context, options: SimpleStreamOptions| {
                calls.fetch_add(1, Ordering::SeqCst);
                scripted(model, context, options)
            },
        );

        let seen = Arc::new(StdMutex::new(Vec::new()));
        let observed = seen.clone();
        let after: AfterToolCallFn = Arc::new(move |after_context| {
            let observed = observed.clone();
            Box::pin(async move {
                observed.lock().push((
                    after_context.tool_call.arguments.clone(),
                    after_context.arguments.clone(),
                    after_context.is_error,
                    after_context.result.content.clone(),
                ));
                Ok(AfterToolCallResult {
                    terminate: Some(true),
                    ..AfterToolCallResult::default()
                })
            })
        });
        let config = AgentLoopConfig {
            model: Model::default(),
            reasoning: ThinkingLevel::Off,
            after_tool_call: Some(after),
            ..AgentLoopConfig::default()
        };
        let (_, abort) = AbortController::new();
        let context = AgentContext {
            system_prompt: String::new(),
            messages: Vec::new(),
            tools: vec![tool],
        };
        let emit: EventSink = Arc::new(|_event| Box::pin(async { Ok(()) }));
        let messages = run_agent_loop(
            vec![Message::user_text("prompt", 1)],
            context,
            config,
            emit,
            stream,
            abort,
        )
        .await
        .expect("run");

        let observed = seen.lock();
        assert_eq!(
            observed.len(),
            1,
            "the after hook must run exactly once for the failed call"
        );
        let (call_arguments, hook_arguments, is_error, content) = &observed[0];
        assert_eq!(*call_arguments, json!({"pattern": "needle", "case": true}));
        assert_eq!(*hook_arguments, json!({"pattern": "needle", "case": true}));
        assert!(*is_error, "schema failure must reach the hook as an error");
        let text = result_text_from_blocks(content);
        assert!(text.starts_with("Invalid grep arguments: "), "{text}");
        assert_eq!(
            stream_calls.load(Ordering::SeqCst),
            1,
            "terminate must stop the loop before the second identical invalid call"
        );
        assert_eq!(
            executed.load(Ordering::SeqCst),
            0,
            "the invalid call must never execute"
        );
        assert!(
            messages.iter().any(|message| matches!(
                message,
                Message::ToolResult(result) if result.is_error
            )),
            "the failed call must produce an error tool result message"
        );
    }

    #[tokio::test]
    async fn hostile_extra_key_never_reaches_the_error_text() {
        let tool = grep_tool();
        let mut arguments = serde_json::Map::new();
        arguments.insert("pattern".to_owned(), json!("needle"));
        arguments.insert("../etc/creds: secret-token".to_owned(), json!(true));
        let error = prepare_rejecting(&tool, Value::Object(arguments)).await;
        let text = result_text(&error);
        assert!(text.contains("additional property not allowed"), "{text}");
        // A colon inside the hostile key would defeat a naive first-separator
        // strip; the stable reason must still expose none of it.
        assert!(!text.contains("secret-token"), "{text}");
        assert!(!text.contains("passwd"), "{text}");
        assert!(!text.contains("../"), "{text}");
        assert!(!text.contains("$."), "{text}");
        assert!(!text.contains("needle"), "{text}");
    }

    #[tokio::test]
    async fn constrained_example_passes_full_schema_validation() {
        let token = Schema {
            schema_type: Some(Value::String("string".into())),
            min_length: Some(3),
            ..Schema::default()
        };
        let nested = Schema::object_ordered(vec![("token".to_owned(), token, true)]);
        let tags = Schema {
            schema_type: Some(Value::String("array".into())),
            items: Some(Box::new(Schema {
                schema_type: Some(Value::String("string".into())),
                min_length: Some(2),
                ..Schema::default()
            })),
            min_items: Some(2),
            ..Schema::default()
        };
        let count = Schema {
            schema_type: Some(Value::String("integer".into())),
            minimum: Some(1.0),
            maximum: Some(10.0),
            ..Schema::default()
        };
        let mode = Schema {
            one_of: vec![
                Schema {
                    schema_type: Some(Value::String("string".into())),
                    enum_values: vec![json!("fast"), json!("slow")],
                    ..Schema::default()
                },
                Schema {
                    schema_type: Some(Value::String("integer".into())),
                    const_value: Some(json!(42)),
                    ..Schema::default()
                },
            ],
            ..Schema::default()
        };
        let schema = Schema::object_ordered(vec![
            ("nested".to_owned(), nested, true),
            ("tags".to_owned(), tags, true),
            ("count".to_owned(), count, true),
            ("mode".to_owned(), mode, true),
        ]);
        let tool = tool_with_schema("prober", schema);
        let error = prepare_rejecting(&tool, json!({"count": 100})).await;
        let text = result_text(&error);
        assert!(text.contains("Corrected example: "), "{text}");
        let example = text
            .split("Corrected example: ")
            .nth(1)
            .expect("example present");
        let skeleton: Value = serde_json::from_str(example).expect("example is JSON");
        pi_ai::validate_tool_arguments(&tool.as_tool_definition(), &skeleton)
            .expect("corrected example must pass the complete schema");
        assert_eq!(skeleton["nested"]["token"], json!("xxx"));
        assert_eq!(skeleton["tags"], json!(["xx", "xx"]));
        assert_eq!(skeleton["count"], json!(1));
        assert_eq!(skeleton["mode"], json!("fast"));
    }

    #[tokio::test]
    async fn unsatisfiable_required_omits_corrected_example() {
        let mut word = Schema::string();
        word.pattern = Some("^a+$".into());
        let schema = Schema::object_ordered(vec![("word".to_owned(), word, true)]);
        let tool = tool_with_schema("prober", schema);
        let error = prepare_rejecting(&tool, json!({})).await;
        let text = result_text(&error);
        assert!(
            text.starts_with("Invalid prober arguments: required property missing."),
            "{text}"
        );
        assert!(
            !text.contains("Corrected example"),
            "an arbitrary pattern must omit the example: {text}"
        );
    }

    #[tokio::test]
    async fn partial_property_order_retains_required_fields() {
        let mut schema = Schema::object_ordered(vec![
            ("required_field".to_owned(), Schema::string(), true),
            ("optional_field".to_owned(), Schema::string(), false),
        ]);
        schema.property_order = vec!["optional_field".to_owned()];
        let tool = tool_with_schema("prober", schema);
        let error = prepare_rejecting(&tool, json!({})).await;
        let text = result_text(&error);
        assert!(text.contains("required property missing"), "{text}");
        assert!(
            text.contains("Allowed fields: optional_field, required_field"),
            "{text}"
        );
        let example = text
            .split("Corrected example: ")
            .nth(1)
            .expect("example present");
        let skeleton: Value = serde_json::from_str(example).expect("example is JSON");
        pi_ai::validate_tool_arguments(&tool.as_tool_definition(), &skeleton)
            .expect("corrected example must pass the complete schema");
        assert_eq!(skeleton["required_field"], json!(""));
        assert_eq!(skeleton["optional_field"], json!(""));
    }

    #[tokio::test]
    async fn parallel_mixed_batch_terminates_and_emits_each_sibling_once() {
        let executed = Arc::new(AtomicUsize::new(0));
        let valid_executed = executed.clone();
        let valid = AgentTool::new(
            "calculator",
            "valid tool",
            Schema::object_ordered(vec![("value".to_owned(), Schema::string(), true)]),
            move |_| {
                let executed = valid_executed.clone();
                async move {
                    executed.fetch_add(1, Ordering::SeqCst);
                    Ok(AgentToolResult::text("42"))
                }
            },
        );
        let never_executed = Arc::new(AtomicUsize::new(0));
        let never_count = never_executed.clone();
        let invalid = AgentTool::new("grep", "strict grep", strict_grep_schema(), move |_| {
            let never_executed = never_count.clone();
            async move {
                never_executed.fetch_add(1, Ordering::SeqCst);
                Ok(AgentToolResult::text("must not run"))
            }
        });
        let mixed_turn = AssistantMessage {
            content: vec![
                ContentBlock::ToolCall(grep_call(
                    "bad-1",
                    json!({"pattern": "needle", "case": true}),
                )),
                ContentBlock::ToolCall(call_for(&valid, "good-1", json!({"value": "8"}))),
            ],
            stop_reason: StopReason::ToolUse,
            ..AssistantMessage::pending(&Model::default())
        };
        let stream_calls = Arc::new(AtomicUsize::new(0));
        let calls = stream_calls.clone();
        let scripted = scripted_assistant(vec![mixed_turn, stop_assistant()]);
        let stream: StreamFn = Arc::new(
            move |model: Model, context: Context, options: SimpleStreamOptions| {
                calls.fetch_add(1, Ordering::SeqCst);
                scripted(model, context, options)
            },
        );
        // The hook only reports terminate for failed calls: the successful
        // sibling must not mask the batch stop request.
        let after: AfterToolCallFn = Arc::new(|after_context| {
            let terminate = after_context.is_error;
            Box::pin(async move {
                Ok(AfterToolCallResult {
                    terminate: Some(terminate),
                    ..AfterToolCallResult::default()
                })
            })
        });
        let events: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
        let observed_events = events.clone();
        let emit: EventSink = Arc::new(move |event| {
            let observed_events = observed_events.clone();
            Box::pin(async move {
                match &event {
                    AgentEvent::ToolExecutionEnd { tool_call_id, .. } => {
                        observed_events
                            .lock()
                            .push(format!("end:{tool_call_id}"));
                    }
                    AgentEvent::MessageEnd { message } => {
                        if let Message::ToolResult(result) = message {
                            observed_events
                                .lock()
                                .push(format!("result:{}", result.tool_call_id));
                        }
                    }
                    _ => {}
                }
                Ok(())
            })
        });
        let config = AgentLoopConfig {
            model: Model::default(),
            reasoning: ThinkingLevel::Off,
            after_tool_call: Some(after),
            ..AgentLoopConfig::default()
        };
        let (_, abort) = AbortController::new();
        let context = AgentContext {
            system_prompt: String::new(),
            messages: Vec::new(),
            tools: vec![invalid, valid],
        };
        let messages = run_agent_loop(
            vec![Message::user_text("prompt", 1)],
            context,
            config,
            emit,
            stream,
            abort,
        )
        .await
        .expect("run");

        assert_eq!(
            stream_calls.load(Ordering::SeqCst),
            1,
            "one terminating failure must stop the next model turn"
        );
        assert_eq!(
            executed.load(Ordering::SeqCst),
            1,
            "the already-prepared sibling must still execute exactly once"
        );
        assert_eq!(
            never_executed.load(Ordering::SeqCst),
            0,
            "the schema-invalid call must never execute"
        );
        let observed = events.lock();
        for id in ["bad-1", "good-1"] {
            assert_eq!(
                observed
                    .iter()
                    .filter(|entry| *entry == &format!("end:{id}"))
                    .count(),
                1,
                "ToolExecutionEnd for {id} must be emitted exactly once"
            );
            assert_eq!(
                observed
                    .iter()
                    .filter(|entry| *entry == &format!("result:{id}"))
                    .count(),
                1,
                "tool result message for {id} must be emitted exactly once"
            );
        }
        assert!(
            messages.iter().any(|message| matches!(
                message,
                Message::ToolResult(result) if result.is_error
            )),
            "the failed sibling must still produce an error tool result message"
        );
    }
}

#[cfg(test)]
mod sequential_terminate_tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use parking_lot::Mutex as StdMutex;
    use pi_ai::{Model, Schema};

    use super::*;
    use crate::{AbortController, AfterToolCallFn, AfterToolCallResult};

    fn counting_tool(name: &'static str, counter: Arc<AtomicUsize>) -> AgentTool {
        AgentTool::new(name, name, Schema::default(), move |_| {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(AgentToolResult::text("ok"))
            }
        })
    }

    fn call_for(name: &str, id: &str) -> ToolCall {
        ToolCall {
            id: id.to_owned(),
            name: name.to_owned(),
            arguments: serde_json::json!({}),
            thought_signature: None,
        }
    }

    #[tokio::test]
    async fn sequential_batch_terminate_stops_before_next_executor() {
        let first_count = Arc::new(AtomicUsize::new(0));
        let second_count = Arc::new(AtomicUsize::new(0));
        let first_tool = counting_tool("first", first_count.clone());
        let second_tool = counting_tool("second", second_count.clone());

        // The after-hook asks the batch to stop once the first call's outcome
        // is final; the second call must never reach an executor.
        let after: AfterToolCallFn = Arc::new(|after_context| {
            let terminate = after_context.tool_call.name == "first";
            Box::pin(async move {
                Ok(AfterToolCallResult {
                    terminate: Some(terminate),
                    ..AfterToolCallResult::default()
                })
            })
        });
        let config = AgentLoopConfig {
            model: Model::default(),
            reasoning: ThinkingLevel::Off,
            after_tool_call: Some(after),
            ..AgentLoopConfig::default()
        };
        let context = AgentContext {
            system_prompt: String::new(),
            messages: Vec::new(),
            tools: vec![first_tool, second_tool],
        };
        let message = AssistantMessage {
            content: vec![
                ContentBlock::ToolCall(call_for("first", "call-1")),
                ContentBlock::ToolCall(call_for("second", "call-2")),
            ],
            stop_reason: StopReason::ToolUse,
            ..AssistantMessage::pending(&Model::default())
        };

        let events: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
        let observed = events.clone();
        let emit: EventSink = Arc::new(move |event| {
            let observed = observed.clone();
            Box::pin(async move {
                match &event {
                    AgentEvent::ToolExecutionEnd { tool_call_id, .. } => {
                        observed.lock().push(format!("end:{tool_call_id}"));
                    }
                    AgentEvent::MessageEnd { message } => {
                        if let Message::ToolResult(result) = message {
                            observed
                                .lock()
                                .push(format!("result:{}", result.tool_call_id));
                        }
                    }
                    _ => {}
                }
                Ok(())
            })
        });

        let (_, abort) = AbortController::new();
        let batch = execute_tool_calls_sequential(
            &context,
            &message,
            &config,
            emit,
            abort,
            vec![call_for("first", "call-1"), call_for("second", "call-2")],
        )
        .await
        .expect("execute sequential batch");

        assert_eq!(
            first_count.load(Ordering::SeqCst),
            1,
            "the first executor must run exactly once"
        );
        assert_eq!(
            second_count.load(Ordering::SeqCst),
            0,
            "the first terminate must prevent the second executor"
        );
        assert!(batch.terminate, "the batch must report the stop request");
        assert_eq!(
            batch.messages.len(),
            1,
            "only the first finalized outcome is batched"
        );
        let observed = events.lock();
        for prefix in ["end:", "result:"] {
            assert_eq!(
                observed
                    .iter()
                    .filter(|entry| *entry == &format!("{prefix}call-1"))
                    .count(),
                1,
                "the first call must emit {prefix}call-1 exactly once"
            );
            assert!(
                !observed.iter().any(|entry| entry == &format!("{prefix}call-2")),
                "the skipped second call must not emit {prefix}call-2"
            );
        }
    }
}
