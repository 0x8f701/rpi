use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use pi_agent::{AgentTool, AgentToolResult, ToolCapability, ToolExecutionMode};
use pi_ai::Schema;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{
    AgentSnapshot, AgentStatus, DeliveryOutcome, JobSnapshot, JobStatus, MailboxMessage,
    OrchestrationRuntime, StructuredOutput, TaskItem, YieldState,
};

const DEFAULT_WAIT_TIMEOUT_MS: u64 = 120_000;
const MAX_TASK_BATCH: usize = 64;

impl OrchestrationRuntime {
    #[must_use]
    pub fn agent_tools(&self, caller_id: &str, depth: usize) -> Vec<AgentTool> {
        let mut tools = Vec::with_capacity(2);
        if depth < self.max_recursion_depth() {
            tools.push(self.task_tool(caller_id, depth));
        }
        tools.push(self.hub_tool(caller_id));
        tools
    }

    fn task_tool(&self, caller_id: &str, depth: usize) -> AgentTool {
        let runtime = self.clone();
        let caller_id = caller_id.to_owned();
        let agents = self
            .enabled_agents()
            .into_iter()
            .map(|agent| format!("{} — {}", agent.name, agent.description))
            .collect::<Vec<_>>()
            .join("\n");
        AgentTool::new(
            "task",
            format!(
                "Start one or more independent child coding-session jobs. Pass a single `task`, optionally with `context`, for one spawn; or pass a `tasks` batch where each child receives background from the top-level shared `context`, its own `tasks[].context`, or both (shared first). `task` and `tasks` are mutually exclusive: pass exactly one of them, never both, and `tasks` must contain at least one item. Every `task` briefing MUST be complete and self-contained — state the objective, the concrete steps, and the acceptance criteria; one-liners or missing acceptance criteria are prohibited. Each batch item may set its own `agent` (which agent type runs it) and `outputSchema`/`schemaMode` (JSON Schema contract its delivered yield payload must satisfy). Returns immediately with stable job and agent ids; use hub jobs/wait/cancel to supervise completion. When a child calls `yield`, that payload becomes the job result. Available agents:\n{}",
                agents
            ),
            task_schema(),
            move |context| {
                let runtime = runtime.clone();
                let caller_id = caller_id.clone();
                async move {
                    let parameters: TaskParameters = serde_json::from_value(context.arguments)
                        .map_err(|error| anyhow!("invalid task arguments: {error}"))?;
                    let items = parameters.into_items(&runtime)?;
                    let spawns = runtime.spawn_tasks(&caller_id, depth, items)?;
                    Ok(AgentToolResult {
                        content: vec![pi_ai::ContentBlock::text(
                            OrchestrationRuntime::task_spawns_text(&spawns),
                        )],
                        details: serde_json::to_value(&spawns)?,
                        ..AgentToolResult::default()
                    })
                }
            },
        )
        .with_capability(ToolCapability::Exec)
        .with_execution_mode(ToolExecutionMode::Sequential)
        .with_prepare_arguments(fill_task_nulls)
    }

    fn hub_tool(&self, caller_id: &str) -> AgentTool {
        let runtime = self.clone();
        let caller_id = caller_id.to_owned();
        AgentTool::new(
            "hub",
            "Coordinate with Main and child peers. Supports send, ask (request/reply with correlation id), wait, inbox, list, read_history, and cancellation/status for child task jobs. This tool does not supervise OS processes.",
            hub_schema(),
            move |context| {
                let runtime = runtime.clone();
                let caller_id = caller_id.clone();
                async move {
                    let parameters: HubParameters = serde_json::from_value(context.arguments)
                        .map_err(|error| anyhow!("invalid hub arguments: {error}"))?;
                    execute_hub(runtime, caller_id, parameters, context.abort).await
                }
            },
        )
        .with_capability(ToolCapability::Read)
        .with_execution_mode(ToolExecutionMode::Sequential)
    }
}

/// Builds the child-only `yield` tool — OMP's explicit-delivery protocol.
///
/// Calling it records the delivered payload and ends the run (the composed
/// turn stop hook fires at the end of the yielding turn, and the run loop
/// projects the payload as the job's final output). The description doubles as
/// the protocol instruction: deliver once at the end, never mid-work. The tool
/// is registered only on orchestration child sessions — the child factory
/// appends it to every child's tool set (plumbing, like task/hub/goal); the
/// main session's tool factories never build it, so a main-session model that
/// calls `yield` gets the standard unknown-tool rejection. The payload is
/// intentionally NOT echoed in the tool result: it lives in the shared
/// [`YieldState`] and becomes the job output, so it crosses the transcript
/// only as the model's own tool-call arguments (redacted at the same
/// presentation boundary as every other tool's arguments).
pub(crate) fn yield_tool(state: Arc<YieldState>) -> AgentTool {
    AgentTool::new(
        "yield",
        "End your work by delivering the final result. Call this exactly once, when the assigned work is complete: pass the full final deliverable as `text` — that payload becomes your delivered output and your session terminates. Do not call it mid-work, and do not continue working after calling it.",
        yield_schema(),
        move |context| {
            let state = state.clone();
            async move {
                let parameters: YieldParameters = serde_json::from_value(context.arguments)
                    .map_err(|error| anyhow!("invalid yield arguments: {error}"))?;
                let delivered = state.deliver(parameters.text);
                Ok(AgentToolResult {
                    content: vec![pi_ai::ContentBlock::text(if delivered {
                        "Delivered. Your task is complete — this session ends now."
                    } else {
                        "yield was already called; this call is ignored."
                    })],
                    ..AgentToolResult::default()
                })
            }
        },
    )
    .with_capability(ToolCapability::Exec)
    .with_execution_mode(ToolExecutionMode::Sequential)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct YieldParameters {
    text: String,
}

fn yield_schema() -> Schema {
    strict_object(vec![(
        "text",
        string_schema(
            "The final deliverable text. This payload becomes the job's delivered output.",
            None,
        ),
        true,
    )])
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskParameters {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    task: Option<String>,
    #[serde(default, rename = "todoTaskId")]
    todo_task_id: Option<String>,
    #[serde(default)]
    context: Option<String>,
    #[serde(default)]
    tasks: Option<Vec<TaskParametersItem>>,
    #[serde(default, rename = "outputSchema")]
    output_schema: Option<Value>,
    #[serde(default, rename = "schemaMode")]
    schema_mode: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskParametersItem {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    context: Option<String>,
    #[serde(default, rename = "todoTaskId")]
    todo_task_id: Option<String>,
    task: String,
    #[serde(default, rename = "outputSchema")]
    output_schema: Option<Value>,
    #[serde(default, rename = "schemaMode")]
    schema_mode: Option<String>,
}

impl TaskParameters {
    pub fn into_items(self, runtime: &OrchestrationRuntime) -> Result<Vec<TaskItem>> {
        match (self.task, self.tasks) {
            (Some(task), None) => {
                if task.trim().is_empty() {
                    bail!("task must not be empty");
                }
                let context = self
                    .context
                    .map(|context| context.trim().to_owned())
                    .filter(|context| !context.is_empty());
                let (output_schema, schema_mode) =
                    validate_output_contract(self.output_schema, self.schema_mode, None)?;
                let agent = runtime.resolve_task_agent(&task, self.agent.as_deref())?;
                Ok(vec![TaskItem {
                    index: 0,
                    id: self
                        .name
                        .filter(|name| !name.trim().is_empty())
                        .unwrap_or_else(|| runtime.generated_agent_id(0)),
                    agent,
                    assignment: task,
                    todo_task_id: self.todo_task_id,
                    context,
                    output_schema,
                    schema_mode,
                    ..TaskItem::default()
                }])
            }
            (None, Some(tasks)) => {
                // A batch whose items carry their own identities is valid even
                // when the model also left top-level name/agent behind
                // (observed GLM retry shape): items lacking their own identity
                // fall back to the top-level values, items that have them win.
                if tasks.is_empty() {
                    bail!("tasks must not be empty");
                }
                if tasks.len() > MAX_TASK_BATCH {
                    bail!("task batch exceeds maximum of {MAX_TASK_BATCH} items");
                }
                let shared = self
                    .context
                    .map(|context| context.trim().to_owned())
                    .filter(|context| !context.is_empty());
                let top_agent = self.agent.as_deref().filter(|agent| !agent.trim().is_empty());
                let top_name = self.name.as_deref().filter(|name| !name.trim().is_empty());
                let batch_len = tasks.len();
                tasks
                    .into_iter()
                    .enumerate()
                    .map(|(index, item)| {
                        if item.task.trim().is_empty() {
                            bail!("tasks[{index}].task must not be empty");
                        }
                        let (output_schema, schema_mode) = validate_output_contract(
                            item.output_schema,
                            item.schema_mode,
                            Some(index),
                        )?;
                        let item_context = item
                            .context
                            .map(|context| context.trim().to_owned())
                            .filter(|context| !context.is_empty());
                        let context = match (shared.as_deref(), item_context.as_deref()) {
                            (Some(shared), Some(item)) => Some(format!("{shared}\n\n{item}")),
                            (Some(shared), None) => Some(shared.to_owned()),
                            (None, Some(item)) => Some(item.to_owned()),
                            (None, None) => bail!(
                                "tasks[{index}].context is required when batch context is absent"
                            ),
                        };
                        let selection_text = format!(
                            "{}\n\n{}",
                            context.as_deref().unwrap_or_default(),
                            item.task
                        );
                        let agent = runtime.resolve_task_agent(
                            &selection_text,
                            item.agent.as_deref().or(top_agent),
                        )?;
                        Ok(TaskItem {
                            index,
                            id: item
                                .name
                                .filter(|name| !name.trim().is_empty())
                                .or_else(|| {
                                    top_name
                                        .filter(|_| batch_len == 1)
                                        .map(|name| name.to_owned())
                                })
                                .unwrap_or_else(|| runtime.generated_agent_id(index)),
                            agent,
                            assignment: item.task,
                            todo_task_id: item.todo_task_id,
                            context,
                            output_schema,
                            schema_mode,
                        })
                    })
                    .collect()
            }
            (Some(_), Some(_)) => bail!("use either task or tasks, not both"),
            (None, None) => bail!(
                "task requires either `task` (the single assignment text) or `tasks` (batch items); \
                 `context` is background only — the assignment text must go in `task` or each tasks[].task"
            ),
        }
    }
}

/// Validate a per-item (or single-spawn) `outputSchema`/`schemaMode` pair.
/// `schemaMode` must be `permissive` or `strict` when present; `outputSchema`
/// must be a JSON Schema object or null and is pre-flighted through the
/// enforceable subset so malformed schemas fail the call actionably instead of
/// surfacing at child settle time.
fn validate_output_contract(
    output_schema: Option<Value>,
    schema_mode: Option<String>,
    index: Option<usize>,
) -> Result<(Option<Value>, Option<String>)> {
    let where_ = index.map_or_else(|| "task".to_owned(), |index| format!("tasks[{index}]"));
    let mode = match schema_mode.as_deref() {
        None | Some("permissive") => "permissive",
        Some("strict") => "strict",
        Some(other) => {
            bail!("{where_}.schemaMode must be \"permissive\" or \"strict\", got {other:?}")
        }
    };
    let schema = match output_schema {
        None | Some(Value::Null) => None,
        Some(schema @ Value::Object(_)) => {
            let parsed = parse_output_schema(&schema).map_err(|error| {
                anyhow!("{where_}.outputSchema is not a supported JSON Schema: {error:#}")
            })?;
            if mode == "strict" {
                validate_required_properties(&parsed, "$", &where_)?;
            }
            Some(schema)
        }
        Some(other) => bail!(
            "{where_}.outputSchema must be a JSON Schema object or null, got {}",
            json_type_name(&other)
        ),
    };
    Ok((schema, mode.eq("strict").then(|| mode.to_owned())))
}

fn validate_required_properties(schema: &Schema, path: &str, where_: &str) -> Result<()> {
    for required in &schema.required {
        if !schema.properties.contains_key(required) {
            bail!(
                "{where_}.outputSchema required property {required:?} is missing from {path}.properties"
            );
        }
    }
    for (name, property) in &schema.properties {
        validate_required_properties(property, &format!("{path}.properties.{name}"), where_)?;
    }
    if let Some(items) = &schema.items {
        validate_required_properties(items, &format!("{path}.items"), where_)?;
    }
    for (keyword, branches) in [
        ("anyOf", &schema.any_of),
        ("oneOf", &schema.one_of),
        ("allOf", &schema.all_of),
    ] {
        for (index, branch) in branches.iter().enumerate() {
            validate_required_properties(branch, &format!("{path}.{keyword}[{index}]"), where_)?;
        }
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct HubParameters {
    op: String,
    #[serde(default)]
    to: Option<String>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    reply_to: Option<String>,
    #[serde(default, rename = "await")]
    await_reply: bool,
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    peek: bool,
    #[serde(default)]
    ids: Option<Vec<String>>,
    #[serde(default)]
    agent_id: Option<String>,
    #[serde(default)]
    lines: Option<usize>,
}

async fn execute_hub(
    runtime: OrchestrationRuntime,
    caller_id: String,
    parameters: HubParameters,
    abort: pi_agent::AbortSignal,
) -> Result<AgentToolResult> {
    match parameters.op.as_str() {
        "send" => {
            let to = required_text(parameters.to, "to")?;
            let message = required_text(parameters.message, "message")?;
            if to == caller_id {
                bail!("cannot send a message to yourself");
            }
            if to == "all" && parameters.await_reply {
                bail!("await is invalid with to=all");
            }
            let (receipts, pending_reply) = if parameters.await_reply {
                let (receipts, waiter, message_id) = runtime.send_with_reply_waiter(
                    &caller_id,
                    &to,
                    &message,
                    parameters.reply_to,
                )?;
                (receipts, Some((waiter, message_id)))
            } else {
                (
                    runtime.send(&caller_id, &to, &message, parameters.reply_to),
                    None,
                )
            };
            let mut lines = receipts
                .iter()
                .map(|receipt| {
                    let label = match receipt.outcome {
                        DeliveryOutcome::Queued => "queued",
                        DeliveryOutcome::Woken => "woken",
                        DeliveryOutcome::Revived => "revived",
                        DeliveryOutcome::Processed => "processed",
                        DeliveryOutcome::Failed => "failed",
                    };
                    let target = match receipt.requested.as_deref() {
                        Some(requested) if requested != receipt.to => {
                            format!("{} (requested {requested})", receipt.to)
                        }
                        _ => receipt.to.clone(),
                    };
                    match (&receipt.error, receipt.outcome) {
                        (Some(error), DeliveryOutcome::Failed) => {
                            format!("- {target}: failed — {error}")
                        }
                        (Some(error), _) => format!("- {target}: {label} — {error}"),
                        (None, _) => format!("- {target}: {label}"),
                    }
                })
                .collect::<Vec<_>>();
            let mut reply_message: Option<MailboxMessage> = None;
            if let Some((waiter, message_id)) = pending_reply
                && let Some(receipt) = receipts.iter().find(|receipt| receipt.error.is_none())
            {
                let await_from = receipt.to.as_str();
                lines.push(String::new());
                let reply = runtime
                    .wait_registered_reply(
                        &caller_id,
                        await_from,
                        &message_id,
                        waiter,
                        Some(Duration::from_millis(
                            parameters.timeout_ms.unwrap_or(DEFAULT_WAIT_TIMEOUT_MS),
                        )),
                        Some(abort),
                    )
                    .await?;
                match &reply {
                    Some(reply) => lines.push(format!("Reply from {}: {}", reply.from, reply.body)),
                    None => lines.push(format!(
                        "No reply to {message_id} from {await_from} before timeout."
                    )),
                }
                reply_message = reply;
            }
            Ok(result(
                if lines.is_empty() {
                    "No recipients.".to_owned()
                } else {
                    lines.join("\n")
                },
                json!({ "op": "send", "receipts": receipts, "reply": reply_message }),
            ))
        }
        "ask" => {
            let requested = required_text(parameters.to, "to")?;
            let message = required_text(parameters.message, "message")?;
            if requested == caller_id {
                bail!("cannot ask yourself");
            }
            if requested == "all" {
                bail!("ask is invalid with to=all; send to one agent");
            }
            let (to, routed_via) = if runtime.is_user_ask_target(&requested) {
                match runtime.user_proxy_agent() {
                    Some(proxy) => (proxy.name.clone(), Some(requested.clone())),
                    None => (requested.clone(), None),
                }
            } else {
                (requested.clone(), None)
            };
            if to == caller_id {
                bail!("cannot ask yourself");
            }
            let (receipts, waiter, message_id) =
                runtime.send_with_reply_waiter(&caller_id, &to, &message, None)?;
            let receipt = receipts.first().cloned();
            let failed = receipt
                .as_ref()
                .is_none_or(|receipt| receipt.error.is_some());
            if failed {
                let detail = receipt
                    .as_ref()
                    .and_then(|receipt| receipt.error.clone())
                    .unwrap_or_else(|| "no recipients".to_owned());
                return Ok(result(
                    format!("Ask to {to} failed — {detail}"),
                    json!({ "op": "ask", "to": to, "routedVia": routed_via, "receipts": receipts, "reply": Value::Null }),
                ));
            }
            let await_from = receipt
                .as_ref()
                .map(|receipt| receipt.to.as_str())
                .unwrap_or(to.as_str());
            let reply = runtime
                .wait_registered_reply(
                    &caller_id,
                    await_from,
                    &message_id,
                    waiter,
                    Some(Duration::from_millis(
                        parameters.timeout_ms.unwrap_or(DEFAULT_WAIT_TIMEOUT_MS),
                    )),
                    Some(abort),
                )
                .await?;
            let text = match &reply {
                Some(reply) => format!(
                    "Ask {message_id} answered by {}: {}",
                    reply.from, reply.body
                ),
                None => format!("No reply to ask {message_id} from {await_from} before timeout."),
            };
            Ok(result(
                text,
                json!({
                    "op": "ask",
                    "to": to,
                    "routedVia": routed_via,
                    "messageId": message_id,
                    "receipts": receipts,
                    "reply": reply,
                }),
            ))
        }
        "wait" if parameters.ids.as_ref().is_some_and(|ids| !ids.is_empty()) => {
            let ids = parameters.ids.expect("ids were checked");
            let jobs = runtime
                .wait_jobs(
                    &ids,
                    parameters.timeout_ms.map(Duration::from_millis),
                    Some(abort),
                )
                .await?;
            Ok(result(
                jobs_text(&jobs, "No job completed before timeout."),
                json!({ "op": "wait", "jobs": jobs }),
            ))
        }
        "wait" => {
            let timeout = parameters.timeout_ms.map(Duration::from_millis);
            let message = runtime
                .wait_message(&caller_id, parameters.from.as_deref(), timeout, Some(abort))
                .await?;
            let text = message.as_ref().map_or_else(
                || "No message before timeout.".to_owned(),
                |message| format!("[{}] {}: {}", message.id, message.from, message.body),
            );
            Ok(result(text, json!({ "op": "wait", "message": message })))
        }
        "inbox" => {
            let messages = runtime.inbox_result(&caller_id, parameters.peek)?;
            let text = if messages.is_empty() {
                "Inbox empty.".to_owned()
            } else {
                messages
                    .iter()
                    .map(|message| format!("[{}] {}: {}", message.id, message.from, message.body))
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            Ok(result(text, json!({ "op": "inbox", "messages": messages })))
        }
        "list" => {
            let peers = runtime.list(&caller_id);
            let text = agents_text(&peers);
            Ok(result(text, json!({ "op": "list", "peers": peers })))
        }
        "jobs" => {
            let jobs = runtime.jobs(parameters.ids.as_deref());
            Ok(result(
                jobs_text(&jobs, "No child jobs."),
                json!({ "op": "jobs", "jobs": jobs }),
            ))
        }
        "cancel" => {
            let ids = parameters
                .ids
                .filter(|ids| !ids.is_empty())
                .ok_or_else(|| anyhow!("ids is required for cancel"))?;
            let cancelled = runtime.cancel_jobs_result(&ids)?;
            Ok(result(
                if cancelled.is_empty() {
                    "No running child tasks matched.".to_owned()
                } else {
                    format!("Cancelled: {}", cancelled.join(", "))
                },
                json!({ "op": "cancel", "cancelled": cancelled }),
            ))
        }
        "read_history" => {
            let agent_id = required_text(parameters.agent_id, "agentId")?;
            let lines = parameters.lines.unwrap_or(super::DEFAULT_HISTORY_LINES);
            if !(1..=super::MAX_HISTORY_LINES).contains(&lines) {
                bail!("lines must be between 1 and {}", super::MAX_HISTORY_LINES);
            }
            let history = runtime.read_child_history(&agent_id, lines)?;
            Ok(result(
                history,
                json!({ "op": "read_history", "agentId": agent_id, "lines": lines }),
            ))
        }
        operation => bail!("unsupported hub operation {operation:?}"),
    }
}

fn agents_text(peers: &[AgentSnapshot]) -> String {
    if peers.is_empty() {
        return "No other agents.".to_owned();
    }
    peers
        .iter()
        .map(|peer| {
            let status = match peer.status {
                AgentStatus::Queued => "queued",
                AgentStatus::Running => "running",
                AgentStatus::Idle => "idle",
                AgentStatus::Parked => "parked",
                AgentStatus::Aborted => "aborted",
            };
            format!(
                "- {} ({}; agent {}) [{}; unread {}; parent {}]",
                peer.id,
                peer.display_name,
                peer.agent,
                status,
                peer.unread,
                peer.parent_id.as_deref().unwrap_or("none")
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn jobs_text(jobs: &[JobSnapshot], empty: &str) -> String {
    if jobs.is_empty() {
        return empty.to_owned();
    }
    jobs.iter()
        .map(|job| {
            let status = match job.status {
                JobStatus::Queued => "queued",
                JobStatus::Running => "running",
                JobStatus::Completed => "completed",
                JobStatus::Failed => "failed",
                JobStatus::Cancelled => "cancelled",
            };
            let result = job.result.as_ref().map_or_else(String::new, |result| {
                if result.output.is_empty() {
                    result
                        .error
                        .as_ref()
                        .map_or_else(String::new, |error| format!(" — {error}"))
                } else {
                    format!(" — {}", result.output)
                }
            });
            format!(
                "- {} [{}; agent {} ({})]{}",
                job.id, status, job.agent_id, job.agent, result
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn result(text: String, details: Value) -> AgentToolResult {
    AgentToolResult {
        content: vec![pi_ai::ContentBlock::text(text)],
        details,
        ..AgentToolResult::default()
    }
}

fn required_text(value: Option<String>, name: &str) -> Result<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("{name} is required"))
}

fn output_schema_field() -> Schema {
    Schema {
        schema_type: Some(Value::String("object".to_owned())),
        description: Some(
            "JSON Schema contract for this child's delivered yield payload: the child must deliver a single JSON value that validates against it. Must be a JSON Schema object or null; booleans are not accepted".to_owned(),
        ),
        ..Schema::default()
    }
}

fn schema_mode_field() -> Schema {
    Schema {
        description: Some(
            "Validation mode for outputSchema: \"permissive\" (report-only, default) or \"strict\" (a validation failure surfaces as a job error)"
                .to_owned(),
        ),
        any_of: vec![
            string_schema("schemaMode value", None).with_enum(["permissive", "strict"]),
            Schema {
                schema_type: Some(Value::String("null".to_owned())),
                ..Schema::default()
            },
        ],
        ..Schema::default()
    }
}

fn task_schema() -> Schema {
    let item = strict_object(vec![
        (
            "name",
            nullable(string_schema("Stable child id", None)),
            true,
        ),
        (
            "agent",
            nullable(string_schema(
                "Agent definition name from the Available agents list in the task tool description; user agents and personas included",
                None,
            )),
            true,
        ),
        (
            "todoTaskId",
            nullable(string_schema(
                "Stable canonical todo task id owned by this child; null for unrelated work. Send JSON null, never the string \"null\"",
                None,
            )),
            true,
        ),
        (
            "context",
            nullable(string_schema(
                "Child-specific background; required when no top-level shared context is provided and appended after shared context when both are present",
                None,
            )),
            true,
        ),
        (
            "task",
            string_schema(
                "Complete, self-contained assignment for this child, including acceptance criteria; never a one-liner",
                Some(1),
            ),
            true,
        ),
        ("outputSchema", nullable(output_schema_field()), true),
        ("schemaMode", schema_mode_field(), true),
    ]);
    strict_object(vec![
        ("name", nullable(string_schema("Stable child id", None)), true),
        ("agent", nullable(string_schema("Agent definition name from the Available agents list in the task tool description; user agents and personas included", None)), true),
        ("task", nullable(string_schema("Complete, self-contained single-child assignment, including acceptance criteria; never a one-liner. Mutually exclusive with `tasks`: pass exactly one of them, never both", Some(1))), true),
        ("todoTaskId", nullable(string_schema("Stable canonical todo task id owned by the single child; null for unrelated work. Send JSON null, never the string \"null\"", None)), true),
        ("context", nullable(string_schema("Optional shared background for a single task or every batch item; batch items may instead or additionally provide tasks[].context", None)), true),
        (
            "tasks",
            nullable(Schema {
                schema_type: Some(Value::String("array".to_owned())),
                description: Some("Independent child assignments (batch, at least one item), mutually exclusive with `task`: pass exactly one of them, never both. Each item needs a complete self-contained task briefing with acceptance criteria and receives context from the shared top-level field, its own context field, or both".to_owned()),
                items: Some(Box::new(item)), min_items: Some(1), max_items: Some(MAX_TASK_BATCH),
                ..Schema::default()
            }),
            true,
        ),
        ("outputSchema", nullable(output_schema_field()), true),
        ("schemaMode", schema_mode_field(), true),
    ])
}

fn nullable(mut schema: Schema) -> Schema {
    schema.nullable = true;
    schema
}

/// Normalize `todoTaskId` in one place: the models keep writing the string
/// "null" where the contract says JSON null. Prepare rewrites it to a real
/// null so the nullable-string schema and the `Option<String>` executor see
/// the same value; any other string stays a literal id.
fn normalize_todo_task_null(arguments: &mut Value) {
    fn clean(object: &mut serde_json::Map<String, Value>, key: &str) {
        let is_null_string =
            matches!(object.get(key), Some(Value::String(text)) if text.trim() == "null");
        if is_null_string {
            object.insert(key.to_owned(), Value::Null);
        }
    }
    if let Some(object) = arguments.as_object_mut() {
        clean(object, "todoTaskId");
        if let Some(Value::Array(tasks)) = object.get_mut("tasks") {
            for task in tasks {
                if let Some(item) = task.as_object_mut() {
                    clean(item, "todoTaskId");
                }
            }
        }
    }
}

fn fill_task_nulls(mut arguments: Value) -> Result<Value> {
    let object = arguments
        .as_object_mut()
        .ok_or_else(|| anyhow!("task arguments must be an object"))?;
    for key in [
        "name",
        "agent",
        "task",
        "todoTaskId",
        "context",
        "tasks",
        "outputSchema",
        "schemaMode",
    ] {
        object.entry(key).or_insert(Value::Null);
    }
    if let Some(tasks) = object.get_mut("tasks").and_then(Value::as_array_mut) {
        for item in tasks {
            let item = item
                .as_object_mut()
                .ok_or_else(|| anyhow!("each task entry must be an object"))?;
            for key in [
                "name",
                "agent",
                "todoTaskId",
                "context",
                "outputSchema",
                "schemaMode",
            ] {
                item.entry(key).or_insert(Value::Null);
            }
        }
    }
    normalize_todo_task_null(&mut arguments);
    Ok(arguments)
}

fn hub_schema() -> Schema {
    // Per-operation discriminator arms. Each arm is a strict object keyed by a
    // const `op`, so the top-level `oneOf` rejects argument shapes that don't
    // satisfy exactly one operation's contract: missing required fields,
    // incompatible cross-op fields, and the `send` `await=true,to=all`
    // combination all fail validation here. The runtime fail-closed checks in
    // `execute_hub` remain as a second layer for callers that bypass schema
    // validation (direct tool-execution tests, serde-driven paths).
    let message = string_schema("Message body", Some(1));
    let timeout_ms = Schema {
        schema_type: Some(Value::String("integer".to_owned())),
        minimum: Some(0.0),
        description: Some("Reply/message wait timeout in milliseconds.".to_owned()),
        ..Schema::default()
    };
    let from = string_schema("Only wait for a message from this sender", Some(1));
    let peek = Schema {
        schema_type: Some(Value::String("boolean".to_owned())),
        description: Some("Peek messages without consuming them.".to_owned()),
        ..Schema::default()
    };
    let ids = Schema {
        schema_type: Some(Value::String("array".to_owned())),
        items: Some(Box::new(string_schema("Job id or child agent id", Some(1)))),
        min_items: Some(1),
        description: Some("Non-empty list of job/agent ids.".to_owned()),
        ..Schema::default()
    };
    let agent_id = string_schema("Agent id whose session transcript to read", Some(1));
    let lines = Schema {
        schema_type: Some(Value::String("integer".to_owned())),
        minimum: Some(1.0),
        maximum: Some(super::MAX_HISTORY_LINES as f64),
        description: Some(format!(
            "Most recent transcript entries to render (1..={}, default 50).",
            super::MAX_HISTORY_LINES
        )),
        ..Schema::default()
    };
    let properties = HashMap::from([
        (
            "op".to_owned(),
            string_schema("Hub operation", Some(1)).with_enum([
                "send",
                "ask",
                "wait",
                "inbox",
                "list",
                "jobs",
                "cancel",
                "read_history",
            ]),
        ),
        (
            "to".to_owned(),
            string_schema("Recipient agent id or all", Some(1)),
        ),
        ("message".to_owned(), message.clone()),
        (
            "replyTo".to_owned(),
            string_schema("Message id being answered", Some(1)),
        ),
        (
            "await".to_owned(),
            Schema {
                schema_type: Some(Value::String("boolean".to_owned())),
                description: Some("Wait for a direct reply when sending.".to_owned()),
                ..Schema::default()
            },
        ),
        ("timeoutMs".to_owned(), timeout_ms.clone()),
        ("from".to_owned(), from.clone()),
        ("peek".to_owned(), peek.clone()),
        ("ids".to_owned(), ids.clone()),
        ("agentId".to_owned(), agent_id.clone()),
        ("lines".to_owned(), lines.clone()),
    ]);

    Schema {
        schema_type: Some(Value::String("object".to_owned())),
        description: Some(
            "Coordinate with Main and child peers. Exactly one operation shape \
             is valid per call: send (optionally awaiting a reply), ask (single \
             recipient, reply required), wait (job ids or a message), inbox, \
             list, jobs, cancel, or read_history."
                .to_owned(),
        ),
        // Anthropic's non-strict tool converter advertises only root
        // properties and required fields. This permissive union keeps every
        // Hub field visible during generation; the oneOf arms below remain
        // the exclusive local validation contract.
        properties,
        property_order: [
            "op",
            "to",
            "message",
            "replyTo",
            "await",
            "timeoutMs",
            "from",
            "peek",
            "ids",
            "agentId",
            "lines",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect(),
        required: vec!["op".to_owned()],
        one_of: vec![
            hub_send_arm(false),
            hub_send_arm(true),
            hub_op_arm(
                "ask",
                vec![
                    ("to", recipient_not_all(), true),
                    ("message", message.clone(), true),
                    ("timeoutMs", timeout_ms.clone(), false),
                ],
            ),
            hub_op_arm(
                "wait",
                vec![
                    ("ids", ids.clone(), true),
                    ("timeoutMs", timeout_ms.clone(), false),
                ],
            ),
            hub_op_arm(
                "wait",
                vec![
                    ("from", from.clone(), false),
                    ("timeoutMs", timeout_ms.clone(), false),
                ],
            ),
            hub_op_arm("inbox", vec![("peek", peek.clone(), false)]),
            hub_op_arm("list", vec![]),
            hub_op_arm("jobs", vec![("ids", ids.clone(), false)]),
            hub_op_arm("cancel", vec![("ids", ids.clone(), true)]),
            hub_op_arm(
                "read_history",
                vec![("agentId", agent_id.clone(), true), ("lines", lines, false)],
            ),
        ],
        ..Schema::default()
    }
}

/// Build a `send` arm. When `await_reply` is true the `to` field excludes the
/// `"all"` broadcast (the `oneOf`-rejects-all trick: `"all"` matches both the
/// `const:"all"` branch and the plain-string branch, so the inner `oneOf`
/// fails with two matches) and `await` is a required `true` const; when
/// false, `await` is an optional `false` const (absent means "do not block")
/// and `to` accepts any non-empty recipient including `"all"`. The two arms
/// are mutually exclusive on the `await` const, so any valid `send` matches
/// exactly one, and `await=true,to=all` matches neither.
fn hub_send_arm(await_reply: bool) -> Schema {
    let (to, await_field, await_required) = if await_reply {
        (
            recipient_not_all(),
            Schema {
                schema_type: Some(Value::String("boolean".to_owned())),
                const_value: Some(Value::Bool(true)),
                description: Some("Block for a reply (forbidden with to=all).".to_owned()),
                ..Schema::default()
            },
            true,
        )
    } else {
        (
            string_schema("Recipient agent id or all", Some(1)),
            Schema {
                schema_type: Some(Value::String("boolean".to_owned())),
                const_value: Some(Value::Bool(false)),
                description: Some("Do not block for a reply (default).".to_owned()),
                ..Schema::default()
            },
            false,
        )
    };
    hub_op_arm(
        "send",
        vec![
            ("to", to, true),
            ("message", string_schema("Message body", Some(1)), true),
            (
                "replyTo",
                string_schema("Message id being answered", Some(1)),
                false,
            ),
            ("await", await_field, await_required),
            (
                "timeoutMs",
                Schema {
                    schema_type: Some(Value::String("integer".to_owned())),
                    minimum: Some(0.0),
                    description: Some("Reply wait timeout in milliseconds.".to_owned()),
                    ..Schema::default()
                },
                false,
            ),
        ],
    )
}

/// A strict object arm discriminated by a const `op` value. The `op` field is
/// always required and fixed; `additionalProperties: false` rejects any field
/// not declared for this operation. Used for every non-`send` operation (and
/// as the inner builder for `hub_send_arm`).
fn hub_op_arm(op: &str, fields: Vec<(&str, Schema, bool)>) -> Schema {
    let mut properties = HashMap::new();
    let mut property_order = Vec::with_capacity(fields.len() + 1);
    let mut required = Vec::new();
    property_order.push("op".to_owned());
    required.push("op".to_owned());
    properties.insert(
        "op".to_owned(),
        Schema {
            schema_type: Some(Value::String("string".to_owned())),
            description: Some(format!("Operation discriminator (always {op:?}).")),
            const_value: Some(Value::String(op.to_owned())),
            ..Schema::default()
        },
    );
    for (name, schema, is_required) in fields {
        property_order.push(name.to_owned());
        if is_required {
            required.push(name.to_owned());
        }
        properties.insert(name.to_owned(), schema);
    }
    Schema {
        schema_type: Some(Value::String("object".to_owned())),
        properties,
        property_order,
        required,
        additional_properties: Some(Value::Bool(false)),
        ..Schema::default()
    }
}

/// A recipient id that must NOT be the `"all"` broadcast. Uses the oneOf
/// reject-all trick: `"all"` matches both the `const:"all"` branch and the
/// plain non-empty string branch, so the surrounding `oneOf` fails with two
/// matches; any other non-empty id matches exactly one branch.
fn recipient_not_all() -> Schema {
    Schema {
        description: Some("Recipient agent id (broadcast to all is not valid here).".to_owned()),
        one_of: vec![
            Schema {
                const_value: Some(Value::String("all".to_owned())),
                ..Schema::default()
            },
            string_schema("Recipient agent id", Some(1)),
        ],
        ..Schema::default()
    }
}

trait SchemaEnumExt {
    fn with_enum<const N: usize>(self, values: [&str; N]) -> Self;
}

impl SchemaEnumExt for Schema {
    fn with_enum<const N: usize>(mut self, values: [&str; N]) -> Self {
        self.enum_values = values
            .into_iter()
            .map(|value| Value::String(value.to_owned()))
            .collect();
        self
    }
}

fn strict_object(fields: Vec<(&str, Schema, bool)>) -> Schema {
    let mut properties = HashMap::new();
    let mut property_order = Vec::new();
    let mut required = Vec::new();
    for (name, schema, is_required) in fields {
        property_order.push(name.to_owned());
        if is_required {
            required.push(name.to_owned());
        }
        properties.insert(name.to_owned(), schema);
    }
    Schema {
        schema_type: Some(Value::String("object".to_owned())),
        properties,
        property_order,
        required,
        additional_properties: Some(Value::Bool(false)),
        ..Schema::default()
    }
}

fn string_schema(description: &str, min_length: Option<usize>) -> Schema {
    Schema {
        schema_type: Some(Value::String("string".to_owned())),
        description: Some(description.to_owned()),
        min_length,
        ..Schema::default()
    }
}

/// Parse an invocation `outputSchema` (a JSON Schema object) into the
/// enforceable [`Schema`] subset used by the payload validator. `$ref`
/// (unresolvable without a schema registry) fails actionably. Unknown keywords
/// round-trip through `Schema::extra` and are skipped by validation, matching
/// the JSON Schema convention that unknown keywords are annotations.
pub(crate) fn parse_output_schema(schema: &Value) -> Result<Schema> {
    match schema {
        Value::Object(object) if object.contains_key("$ref") => {
            bail!("outputSchema $ref is not supported")
        }
        Value::Object(_) => {
            serde_json::from_value(schema.clone()).map_err(|error| anyhow!("{error}"))
        }
        other => bail!(
            "outputSchema must be a JSON Schema object or null, got {}",
            json_type_name(other)
        ),
    }
}

/// Validate a child's delivered `yield` payload against the invocation's
/// per-item `outputSchema` contract (OMP `outputSchema`/`schemaMode` parity).
/// Returns `None` when the item carried no contract or the run itself failed
/// (an errored run's trailing text is not a deliverable to validate). The
/// report carries the parsed payload (when it was JSON) so the parent can
/// inspect what the child delivered even when validation rejected it.
pub(crate) fn validate_delivered_output(
    payload: &str,
    output_schema: Option<&Value>,
    schema_mode: Option<&str>,
    run_error: Option<&str>,
) -> Option<StructuredOutput> {
    let schema_value = output_schema?;
    if run_error.is_some() {
        return None;
    }
    let mode = schema_mode.unwrap_or("permissive");
    let (valid, data, error) = match serde_json::from_str::<Value>(payload) {
        Err(parse_error) => (
            false,
            None,
            Some(format!(
                "delivered payload is not valid JSON: {parse_error}"
            )),
        ),
        Ok(data) => {
            match parse_output_schema(schema_value).and_then(|schema| schema.validate(&data)) {
                Ok(validated) => (true, Some(validated), None),
                Err(validation_error) => (
                    false,
                    Some(data),
                    Some(format!(
                        "delivered payload failed outputSchema validation: {validation_error:#}"
                    )),
                ),
            }
        }
    };
    Some(StructuredOutput {
        schema_source: "task".to_owned(),
        schema_mode: mode.to_owned(),
        valid,
        data,
        error,
    })
}

fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod advertisement_tests {
    use super::*;
    use crate::{
        AgentCatalog, AgentDefinition, AgentDefinitionSource, AgentRuntimeSettings,
        OrchestrationConfig,
    };
    use pi_agent::ThinkingLevel;
    use std::sync::Arc;

    fn def(name: &str) -> AgentDefinition {
        AgentDefinition {
            name: name.to_owned(),
            description: format!("{name} description"),
            system_prompt: "p".to_owned(),
            tools: Some(Vec::new()),
            autoload_skills: Vec::new(),
            model: None,
            thinking_level: Some(ThinkingLevel::Off),
            max_turns: None,
            max_tool_calls: None,
            timeout_secs: None,
            disallowed_tools: Vec::new(),
            capability_ceiling: None,
            source: AgentDefinitionSource::Bundled,
            path: None,
            trusted: true,
            kind: crate::AgentDefinitionKind::Agent,
            personality: None,
            soft_budget: None,
            knowledge_roots: Vec::new(),
            user_proxy: false,
        }
    }

    #[test]
    fn disabled_agents_excluded_from_task_description() {
        let dir = tempfile::tempdir().expect("artifacts");
        let mut config = OrchestrationConfig::new(
            AgentCatalog::from_agents(vec![def("task"), def("reviewer")]),
            dir.path(),
        );
        config.agent_settings.insert(
            "reviewer".to_owned(),
            AgentRuntimeSettings {
                enabled: Some(false),
                model: None,
                tools: None,
            },
        );
        let runtime =
            OrchestrationRuntime::new(config, Arc::new(|_| Box::pin(async { panic!("unused") })))
                .expect("runtime");
        let tools = runtime.agent_tools("Main", 0);
        let task = tools
            .iter()
            .find(|tool| tool.name == "task")
            .expect("task tool");
        assert!(task.description.contains("task —"));
        assert!(
            !task.description.contains("reviewer —"),
            "{}",
            task.description
        );
        assert_eq!(runtime.select_agent("anything", None), "task");
    }

    #[test]
    fn orchestration_tools_carry_explicit_capabilities() {
        let dir = tempfile::tempdir().expect("artifacts");
        let runtime = OrchestrationRuntime::new(
            OrchestrationConfig::new(AgentCatalog::from_agents(vec![def("task")]), dir.path()),
            Arc::new(|_| Box::pin(async { panic!("unused") })),
        )
        .expect("runtime");
        let tools = runtime.agent_tools("Main", 0);
        assert_eq!(
            tools
                .iter()
                .find(|tool| tool.name == "task")
                .unwrap()
                .capability,
            ToolCapability::Exec
        );
        assert_eq!(
            tools
                .iter()
                .find(|tool| tool.name == "hub")
                .unwrap()
                .capability,
            ToolCapability::Read
        );
    }

    #[test]
    fn human_agent_list_distinguishes_id_display_name_and_type() {
        let peers = vec![AgentSnapshot {
            id: "review-job".to_owned(),
            display_name: "Code Review".to_owned(),
            agent: "reviewer".to_owned(),
            parent_id: Some("Main".to_owned()),
            status: AgentStatus::Running,
            created_at: 1,
            last_activity: 2,
            unread: 3,
            artifact_ref: None,
            history_ref: None,
        }];

        assert_eq!(
            agents_text(&peers),
            "- review-job (Code Review; agent reviewer) [running; unread 3; parent Main]"
        );
        assert_eq!(agents_text(&[]), "No other agents.");
    }

    #[test]
    fn task_schema_is_valid_for_openai_strict_tools() {
        let schema = serde_json::to_value(task_schema()).expect("task schema");
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["additionalProperties"], false);
        let properties = schema["properties"].as_object().expect("task properties");
        let required = schema["required"].as_array().expect("task required");
        assert_eq!(required.len(), properties.len());
        for name in properties.keys() {
            assert!(required.iter().any(|item| item == name));
        }
        let prepared = fill_task_nulls(json!({"task":"inspect source"})).expect("prepare task");
        assert!(task_schema().validate(&prepared).is_ok());
    }
    #[test]
    fn task_schema_identifies_available_agent_source() {
        let schema = serde_json::to_value(task_schema()).expect("task schema");
        for description in [
            schema["properties"]["agent"]["description"].as_str(),
            schema["properties"]["tasks"]["items"]["properties"]["agent"]["description"].as_str(),
        ] {
            let description = description.expect("agent description");
            assert!(
                description.contains("Available agents list"),
                "{description}"
            );
            assert!(
                description.contains("user agents and personas included"),
                "{description}"
            );
        }
    }

    #[test]
    fn task_schema_constrains_schema_mode_values() {
        let schema = serde_json::to_value(task_schema()).expect("task schema");
        for mode in [
            &schema["properties"]["schemaMode"],
            &schema["properties"]["tasks"]["items"]["properties"]["schemaMode"],
        ] {
            assert_eq!(mode["anyOf"][0]["enum"], json!(["permissive", "strict"]));
            assert_eq!(mode["anyOf"][0]["type"], "string");
            assert_eq!(mode["anyOf"][1]["type"], "null");
        }
        for value in [Value::Null, json!("permissive"), json!("strict")] {
            let prepared = fill_task_nulls(json!({"task": "inspect source", "schemaMode": value}))
                .expect("prepare task");
            assert!(task_schema().validate(&prepared).is_ok(), "{prepared}");
        }
        let invalid = fill_task_nulls(json!({"task": "inspect source", "schemaMode": "invalid"}))
            .expect("prepare invalid task");
        assert!(task_schema().validate(&invalid).is_err());
    }
}

#[cfg(test)]
mod task_parameters_tests {
    use super::*;
    use crate::{AgentCatalog, AgentDefinition, AgentDefinitionSource, OrchestrationConfig};
    use pi_agent::ThinkingLevel;
    use std::sync::Arc;

    fn def(name: &str) -> AgentDefinition {
        AgentDefinition {
            name: name.to_owned(),
            description: format!("{name} description"),
            system_prompt: "p".to_owned(),
            tools: Some(Vec::new()),
            autoload_skills: Vec::new(),
            model: None,
            thinking_level: Some(ThinkingLevel::Off),
            max_turns: None,
            max_tool_calls: None,
            timeout_secs: None,
            disallowed_tools: Vec::new(),
            capability_ceiling: None,
            source: AgentDefinitionSource::Bundled,
            path: None,
            trusted: true,
            kind: crate::AgentDefinitionKind::Agent,
            personality: None,
            soft_budget: None,
            knowledge_roots: Vec::new(),
            user_proxy: false,
        }
    }

    fn runtime(names: &[&str]) -> OrchestrationRuntime {
        let dir = tempfile::tempdir().expect("artifacts");
        OrchestrationRuntime::new(
            OrchestrationConfig::new(
                AgentCatalog::from_agents(names.iter().map(|name| def(name)).collect()),
                dir.path(),
            ),
            Arc::new(|_| Box::pin(async { panic!("unused") })),
        )
        .expect("runtime")
    }
    fn reviewer_def() -> AgentDefinition {
        let mut definition = def("reviewer");
        definition.tools = Some(vec!["read".to_owned(), "grep".to_owned()]);
        definition.capability_ceiling = Some(crate::CapabilityCeiling {
            read: true,
            write: false,
            exec: false,
        });
        definition
    }

    fn execution_runtime(
        artifact_dir: &std::path::Path,
        captured: Arc<
            parking_lot::Mutex<
                Option<(
                    String,
                    String,
                    Option<Vec<String>>,
                    Option<crate::CapabilityCeiling>,
                    Option<String>,
                )>,
            >,
        >,
        release: Arc<tokio::sync::Notify>,
        delivered: Value,
    ) -> OrchestrationRuntime {
        OrchestrationRuntime::new(
            OrchestrationConfig::new(
                AgentCatalog::from_agents(vec![def("task"), reviewer_def()]),
                artifact_dir,
            ),
            Arc::new(move |request| {
                *captured.lock() = Some((
                    request.system_prompt.clone(),
                    request.definition.name.clone(),
                    request.requested_tool_names.clone(),
                    request.definition.capability_ceiling,
                    request.schema_mode.clone(),
                ));
                let release = release.clone();
                let delivered = delivered.clone();
                Box::pin(async move {
                    release.notified().await;
                    request
                        .yield_state
                        .deliver(serde_json::to_string(&delivered).expect("delivered JSON"));
                    let stream_fn: pi_agent::StreamFn = Arc::new(|model, _context, _options| {
                        Box::pin(async move {
                            let mut message = pi_ai::AssistantMessage::pending(&model);
                            message.content = vec![pi_ai::ContentBlock::text("review settled")];
                            message.stop_reason = pi_ai::StopReason::Stop;
                            let stream = pi_ai::new_assistant_message_event_stream();
                            let producer = stream.clone();
                            tokio::spawn(async move {
                                producer
                                    .push(pi_ai::AssistantMessageEvent::Start {
                                        partial: pi_ai::AssistantMessage::pending(&model),
                                    })
                                    .await;
                                producer
                                    .push(pi_ai::AssistantMessageEvent::Done {
                                        reason: pi_ai::StopReason::Stop,
                                        message: message.clone(),
                                    })
                                    .await;
                                producer.end(Some(message)).await;
                            });
                            stream
                        })
                    });
                    crate::Session::new(crate::SessionOptions {
                        model: request.model,
                        cwd: std::env::current_dir().expect("cwd"),
                        system_prompt: request.system_prompt,
                        thinking_level: pi_agent::ThinkingLevel::Off,
                        api_key: String::new(),
                        compaction: None,
                        stream_options: Default::default(),
                        tools: Some(Vec::new()),
                        before_tool_call: None,
                        after_tool_call: None,
                        stream_fn: Some(stream_fn),
                        auth_resolver: None,
                        auth_manager: None,
                    })
                })
            }),
        )
        .expect("runtime")
    }

    fn task_context(arguments: Value) -> pi_agent::ToolCallContext {
        let (_controller, abort) = pi_agent::AbortController::new();
        pi_agent::ToolCallContext {
            tool_call_id: "goal-task-launch".to_owned(),
            arguments,
            on_update: Arc::new(|_| {}),
            abort,
            model: None,
            approval_token: None,
        }
    }

    fn parameters(value: Value) -> TaskParameters {
        serde_json::from_value(value).expect("task parameters")
    }

    #[test]
    fn single_spawn_keeps_flat_shape_without_context() {
        let items = parameters(json!({"task": "inspect source"}))
            .into_items(&runtime(&["task"]))
            .expect("items");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].index, 0);
        assert_eq!(items[0].assignment, "inspect source");
        assert_eq!(items[0].context, None);
        assert_eq!(items[0].output_schema, None);
        assert_eq!(items[0].schema_mode, None);
    }

    #[test]
    fn single_spawn_carries_shared_context() {
        let items = parameters(json!({"task": "inspect", "context": "shared"}))
            .into_items(&runtime(&["task"]))
            .expect("single context must be accepted");
        assert_eq!(items[0].context.as_deref(), Some("shared"));
    }

    #[test]
    fn single_spawn_carries_output_contract() {
        let items = parameters(json!({
            "task": "deliver a JSON report",
            "outputSchema": {"type": "object", "required": ["summary"], "properties": {"summary": {"type": "string"}}},
            "schemaMode": "strict",
        }))
        .into_items(&runtime(&["task"]))
        .expect("items");
        assert_eq!(
            items[0]
                .output_schema
                .as_ref()
                .and_then(|s| s.get("type"))
                .and_then(Value::as_str),
            Some("object")
        );
        assert_eq!(items[0].schema_mode.as_deref(), Some("strict"));
    }

    #[test]
    fn exact_reviewer_payload_rejects_inconsistent_strict_schema_before_spawn() {
        let runtime = runtime(&["task", "reviewer"]);
        let exact_payload = json!({
            "name": "final-rollback-review",
            "agent": "reviewer",
            "context": "Review the complete rollback design against source.",
            "task": "Perform the meticulous final review with exact evidence and corrections.",
            "outputSchema": {
                "type": "object",
                "properties": {
                    "findings": { "type": "array", "items": { "type": "object" } }
                },
                "required": ["findings", "summary"],
                "summary": { "type": "string" }
            },
            "schemaMode": "strict"
        });
        let error = parameters(exact_payload)
            .into_items(&runtime)
            .expect_err("inconsistent required property must fail before spawn");

        assert_eq!(
            error.to_string(),
            "task.outputSchema required property \"summary\" is missing from $.properties"
        );
        assert!(
            runtime.jobs(None).is_empty(),
            "preflight failure must create zero jobs"
        );
        assert!(
            runtime.list("Main").is_empty(),
            "preflight failure must create zero children"
        );
    }

    #[test]
    fn single_reviewer_spawn_carries_context_and_strict_schema() {
        let schema = json!({
            "type": "object",
            "properties": {
                "findings": { "type": "array", "items": { "type": "object" } },
                "summary": { "type": "string" }
            },
            "required": ["findings", "summary"]
        });
        let items = parameters(json!({
            "name": "final-rollback-review",
            "agent": "reviewer",
            "context": "Review the complete rollback design against source.",
            "task": "Perform the meticulous final review with exact evidence and corrections.",
            "outputSchema": schema,
            "schemaMode": "strict"
        }))
        .into_items(&runtime(&["task", "reviewer"]))
        .expect("single reviewer task with context and strict schema");

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "final-rollback-review");
        assert_eq!(items[0].agent, "reviewer");
        assert_eq!(
            items[0].context.as_deref(),
            Some("Review the complete rollback design against source.")
        );
        assert_eq!(items[0].schema_mode.as_deref(), Some("strict"));
        assert_eq!(items[0].output_schema.as_ref(), Some(&schema));
    }

    #[test]
    fn batch_requires_context_for_every_item() {
        let error = parameters(json!({"tasks": [{"task": "one"}]}))
            .into_items(&runtime(&["task"]))
            .expect_err("batch item without any context must fail");
        assert!(
            error.to_string().contains("tasks[0].context is required"),
            "{error:#}"
        );
        let error = parameters(json!({"context": "   ", "tasks": [{"task": "one"}]}))
            .into_items(&runtime(&["task"]))
            .expect_err("blank shared context must fail without item context");
        assert!(
            error.to_string().contains("tasks[0].context is required"),
            "{error:#}"
        );
    }

    #[test]
    fn batch_carries_shared_context_and_keeps_assignment_as_own_task() {
        let items = parameters(json!({
            "context": "shared background",
            "tasks": [
                {"name": "A", "task": "alpha work", "agent": "task"},
                {"name": "B", "task": "beta work"},
            ],
        }))
        .into_items(&runtime(&["task"]))
        .expect("items");
        assert_eq!(items.len(), 2);
        for item in &items {
            assert_eq!(item.context.as_deref(), Some("shared background"));
        }
        // OMP parity: the child's assignment is its own task briefing only;
        // the shared context lives in the system prompt's CONTEXT section.
        assert_eq!(items[0].assignment, "alpha work");
        assert_eq!(items[1].assignment, "beta work");
        assert!(!items[0].assignment.contains("shared background"));
    }

    #[test]
    fn batch_accepts_reported_per_item_context_payload() {
        let payload = json!({
            "tasks": [
                {
                    "agent": "reviewer",
                    "context": "Repository sample-sdk. Review read-only with exact evidence.",
                    "name": "sdk-quality-review",
                    "task": "Perform a thorough review with prioritized findings and acceptance criteria."
                },
                {
                    "agent": "scout",
                    "context": "Repository sample-sdk. Audit build and test readiness read-only.",
                    "name": "sdk-test-build-audit",
                    "task": "Inspect manifests and run safe finite checks with exact command results."
                }
            ]
        });
        let prepared = fill_task_nulls(payload.clone()).expect("prepare reported payload");
        task_schema()
            .validate(&prepared)
            .expect("per-item context is declared by the strict schema");
        let items = parameters(payload)
            .into_items(&runtime(&["task", "reviewer", "scout"]))
            .expect("parse reported payload");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].id, "sdk-quality-review");
        assert_eq!(
            items[0].context.as_deref(),
            Some("Repository sample-sdk. Review read-only with exact evidence.")
        );
        assert_eq!(items[1].id, "sdk-test-build-audit");
        assert_eq!(
            items[1].context.as_deref(),
            Some("Repository sample-sdk. Audit build and test readiness read-only.")
        );
    }

    #[test]
    fn batch_combines_shared_and_item_context_in_order() {
        let items = parameters(json!({
            "context": "shared repository constraints",
            "tasks": [{"task": "review one module", "context": "module-specific evidence"}],
        }))
        .into_items(&runtime(&["task"]))
        .expect("combined context");
        assert_eq!(
            items[0].context.as_deref(),
            Some("shared repository constraints\n\nmodule-specific evidence")
        );
    }

    #[test]
    fn batch_selects_agent_per_item() {
        let items = parameters(json!({
            "context": "shared",
            "tasks": [
                {"task": "plain coding work", "agent": "task"},
                {"task": "review the diff", "agent": "reviewer"},
            ],
        }))
        .into_items(&runtime(&["task", "reviewer"]))
        .expect("items");
        assert_eq!(items[0].agent, "task");
        assert_eq!(items[1].agent, "reviewer");
        // Top-level name/agent alongside per-item identities: items lacking
        // their own fall back to the top-level values (observed GLM retry
        // shape) — a bogus name still fails through the catalog check.
        let repaired = parameters(json!({"agent": "reviewer", "context": "c", "tasks": [{"task": "t"}]}))
            .into_items(&runtime(&["task", "reviewer"]))
            .expect("item without agent falls back to top-level agent");
        assert_eq!(repaired[0].agent, "reviewer");
        let item_wins = parameters(json!({
            "agent": "reviewer",
            "context": "c",
            "tasks": [{"task": "t", "agent": "task"}],
        }))
        .into_items(&runtime(&["task", "reviewer"]))
        .expect("item agent wins over top-level fallback");
        assert_eq!(item_wins[0].agent, "task");
        let bogus = parameters(json!({"agent": "no-such-agent", "context": "c", "tasks": [{"task": "t"}]}))
            .into_items(&runtime(&["task", "reviewer"]));
        assert!(bogus.is_err(), "bogus fallback name must still fail through the catalog");
    }

    #[test]
    fn batch_carries_per_item_output_contract() {
        let items = parameters(json!({
            "context": "shared",
            "tasks": [
                {

                    "task": "deliver JSON",
                    "outputSchema": {"type": "object", "properties": {"ok": {"type": "boolean"}}, "required": ["ok"]},
                    "schemaMode": "strict",
                },
                {"task": "plain text"},
            ],
        }))
        .into_items(&runtime(&["task"]))
        .expect("items");
        assert_eq!(
            items[0]
                .output_schema
                .as_ref()
                .and_then(|s| s.get("type"))
                .and_then(Value::as_str),
            Some("object")
        );
        assert_eq!(items[0].schema_mode.as_deref(), Some("strict"));
        assert_eq!(items[1].output_schema, None);
        assert_eq!(items[1].schema_mode, None);
        // An omitted schemaMode with a schema is fine (defaults permissive).
        assert_eq!(items[1].schema_mode, None);
    }
    #[tokio::test]
    async fn exact_invalid_goal_task_execution_creates_no_authoritative_child() {
        let runtime = runtime(&["task", "reviewer"]);
        let tool = runtime.task_tool("Main", 0);
        let exact_payload = json!({
            "name": "final-rollback-review",
            "agent": "reviewer",
            "context": "Review the complete rollback design against source.",
            "task": "Perform the meticulous final review with exact evidence and corrections.",
            "outputSchema": {
                "type": "object",
                "properties": {
                    "findings": { "type": "array", "items": { "type": "object" } }
                },
                "required": ["findings", "summary"],
                "summary": { "type": "string" }
            },
            "schemaMode": "strict"
        });
        let prepared = (tool.prepare_arguments.as_ref().expect("task prepare"))(exact_payload)
            .expect("prepare exact payload");
        let error = (tool.execute)(task_context(prepared))
            .await
            .expect_err("inconsistent strict schema must reject the Task call");

        assert_eq!(
            error.to_string(),
            "task.outputSchema required property \"summary\" is missing from $.properties"
        );
        assert!(
            runtime.jobs(None).is_empty(),
            "Task execution must create zero jobs"
        );
        assert!(
            runtime.list("Main").is_empty(),
            "Task execution must create zero children"
        );
    }

    #[tokio::test]
    async fn valid_goal_task_execution_launches_authoritative_reviewer_and_settles_strict_result() {
        let artifacts = tempfile::tempdir().expect("artifacts");
        let captured = Arc::new(parking_lot::Mutex::new(None));
        let release = Arc::new(tokio::sync::Notify::new());
        let runtime = execution_runtime(
            artifacts.path(),
            captured.clone(),
            release.clone(),
            json!({"findings": [], "summary": "review complete"}),
        );
        let tool = runtime.task_tool("Main", 0);
        let payload = json!({
            "name": "final-rollback-review",
            "agent": "reviewer",
            "context": "Review the complete rollback design against source.",
            "task": "Perform the meticulous final review with exact evidence and corrections.",
            "outputSchema": {
                "type": "object",
                "properties": {
                    "findings": { "type": "array", "items": { "type": "object" } },
                    "summary": { "type": "string" }
                },
                "required": ["findings", "summary"]
            },
            "schemaMode": "strict"
        });
        let prepared = (tool.prepare_arguments.as_ref().expect("task prepare"))(payload)
            .expect("prepare valid payload");
        let launch = (tool.execute)(task_context(prepared))
            .await
            .expect("launch Task");
        let spawns: Vec<super::super::TaskSpawn> =
            serde_json::from_value(launch.details).expect("authoritative Task spawns");
        assert_eq!(spawns.len(), 1);
        assert_eq!(spawns[0].agent_id, "final-rollback-review");
        assert_eq!(spawns[0].agent, "reviewer");

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if captured.lock().is_some() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("reviewer factory capture");
        let (system_prompt, definition_name, requested_tools, capability_ceiling, schema_mode) =
            captured.lock().clone().expect("captured request");
        assert!(system_prompt.contains("Review the complete rollback design against source."));
        assert_eq!(definition_name, "reviewer");
        assert_eq!(schema_mode.as_deref(), Some("strict"));
        assert_eq!(
            capability_ceiling,
            Some(crate::CapabilityCeiling {
                read: true,
                write: false,
                exec: false,
            })
        );
        assert_eq!(
            requested_tools.as_deref(),
            Some(["read".to_owned(), "grep".to_owned()].as_slice())
        );
        let running = runtime.jobs(Some(&[spawns[0].job_id.clone()]));
        assert_eq!(running.len(), 1);
        assert_eq!(running[0].agent_id, spawns[0].agent_id);
        assert!(matches!(
            running[0].status,
            JobStatus::Queued | JobStatus::Running
        ));
        assert!(
            running[0].result.is_none(),
            "parent Task cannot settle before reviewer"
        );

        release.notify_one();
        let settled = runtime
            .wait_jobs(
                &[spawns[0].job_id.clone()],
                Some(Duration::from_secs(2)),
                None,
            )
            .await
            .expect("reviewer settlement");
        assert_eq!(settled.len(), 1);
        assert_eq!(settled[0].id, spawns[0].job_id);
        assert_eq!(settled[0].agent_id, spawns[0].agent_id);
        assert_eq!(settled[0].status, JobStatus::Completed);
        let task_result = settled[0].result.as_ref().expect("settled Task result");
        assert_eq!(
            task_result.output,
            r#"{"findings":[],"summary":"review complete"}"#
        );
        assert!(
            task_result.error.is_none(),
            "strict result: {:?}",
            task_result.error
        );
        assert_eq!(
            task_result
                .structured_output
                .as_ref()
                .and_then(|output| output.data.clone()),
            Some(json!({"findings": [], "summary": "review complete"}))
        );
    }

    #[test]
    fn schema_mode_must_be_permissive_or_strict() {
        let error = parameters(json!({
            "context": "c",
            "tasks": [{"task": "t", "schemaMode": "bogus"}],
        }))
        .into_items(&runtime(&["task"]))
        .expect_err("invalid schemaMode");
        assert!(error.to_string().contains("schemaMode"), "{error:#}");
    }

    #[test]
    fn malformed_output_schema_fails_the_call() {
        for bad in [
            json!("not-a-schema"),
            json!(false),
            json!({"$ref": "#/definitions/x"}),
            json!(42),
        ] {
            let error = parameters(json!({
                "context": "c",
                "tasks": [{"task": "t", "outputSchema": bad}],
            }))
            .into_items(&runtime(&["task"]))
            .expect_err("malformed outputSchema");
            assert!(error.to_string().contains("outputSchema"), "{error:#}");
        }
    }

    #[test]
    fn task_schema_exposes_output_contract_fields() {
        let schema = serde_json::to_value(task_schema()).expect("task schema");
        let top = schema["properties"].as_object().expect("top properties");
        assert!(top.contains_key("outputSchema"), "top-level outputSchema");
        assert!(top.contains_key("schemaMode"), "top-level schemaMode");
        let item = schema["properties"]["tasks"]["items"]["properties"]
            .as_object()
            .expect("item properties");
        assert!(item.contains_key("outputSchema"), "item outputSchema");
        assert!(item.contains_key("schemaMode"), "item schemaMode");
        // The strict-object schema stays fillable with the new nullable keys.
        let prepared = fill_task_nulls(json!({
            "context": "c",
            "tasks": [{"task": "t"}],
        }))
        .expect("prepare");
        assert!(
            task_schema().validate(&prepared).is_ok(),
            "validated batch args"
        );
    }

    #[test]
    fn delivered_payload_is_validated_against_the_schema() {
        let schema = json!({
            "type": "object",
            "properties": {"ok": {"type": "boolean"}},
            "required": ["ok"],
        });
        // Conforming JSON payload → valid, parsed data surfaced.
        let report =
            validate_delivered_output(r#"{"ok": true}"#, Some(&schema), Some("strict"), None)
                .expect("report");
        assert!(report.valid, "conforming payload must validate");
        assert_eq!(report.schema_source, "task");
        assert_eq!(report.schema_mode, "strict");
        assert_eq!(report.data, Some(json!({"ok": true})));
        assert_eq!(report.error, None);

        // Non-conforming JSON → invalid with a path-annotated error; the
        // parsed data is still surfaced for inspection.
        let report = validate_delivered_output(r#"{"ok": "nope"}"#, Some(&schema), None, None)
            .expect("report");
        assert!(!report.valid, "non-conforming payload must fail");
        assert_eq!(
            report.schema_mode, "permissive",
            "absent mode defaults to permissive"
        );
        assert_eq!(report.data, Some(json!({"ok": "nope"})));
        assert!(
            report
                .error
                .as_deref()
                .is_some_and(|error| error.contains("outputSchema"))
        );

        // Non-JSON payload → invalid with a parse error and no data.
        let report = validate_delivered_output("not json at all", Some(&schema), None, None)
            .expect("report");
        assert!(!report.valid);
        assert_eq!(report.data, None);
        assert!(
            report
                .error
                .as_deref()
                .is_some_and(|error| error.contains("not valid JSON"))
        );

        // No contract → no report; an errored run → no report either.
        assert!(validate_delivered_output("anything", None, None, None).is_none());
        assert!(
            validate_delivered_output("anything", Some(&schema), None, Some("run failed"))
                .is_none()
        );

    }

    /// User-reported failure 1: outputSchema=true was rejected by the
    /// boolean-naive gate while schema descriptions induced it. The single
    /// contract is `object|null`, enforced at the schema gate and fail-closed
    /// again in the executor.
    #[test]
    fn output_schema_true_and_string_forms_fail_with_exact_contract_error() {
        let runtime = runtime(&["task"]);
        for payload in [
            json!({"task": "deliver a structured report", "outputSchema": true}),
            json!({"task": "deliver a structured report", "outputSchema": "true"}),
            json!({"task": "deliver a structured report", "outputSchema": false}),
        ] {
            let prepared = fill_task_nulls(payload.clone()).expect("prepare");
            let error = task_schema().validate(&prepared).expect_err(&format!(
                "non-object outputSchema must fail the schema gate: {payload}"
            ));
            assert_eq!(
                error.to_string(),
                "$.outputSchema: expected object or null",
                "{payload}"
            );
            let error = parameters(payload.clone())
                .into_items(&runtime)
                .expect_err("executor must fail closed on non-object outputSchema");
            assert!(
                error.to_string().contains(
                    "outputSchema must be a JSON Schema object or null, got",
                ),
                "{error:#}"
            );
        }
        assert!(
            runtime.jobs(None).is_empty(),
            "rejected outputSchema must dispatch zero jobs"
        );
        assert!(
            runtime.list("Main").is_empty(),
            "rejected outputSchema must spawn zero children"
        );
    }

    /// User-reported failure 2: top-level `task` and `tasks[]` in one call.
    /// The nullable-field strict schema (all keys null-filled for OpenAI
    /// strict mode) cannot express XOR without losing precise per-case
    /// remediation, so the schema states it and `TaskParameters::into_items`
    /// rejects it actionably before any spawn.
    #[test]
    fn combined_task_and_tasks_fail_without_dispatch() {
        let runtime = runtime(&["task"]);
        let payload = json!({
            "task": "solo assignment with acceptance criteria",
            "context": "shared background",
            "tasks": [{"task": "batch assignment with acceptance criteria", "context": "item"}],
        });
        let error = parameters(payload)
            .into_items(&runtime)
            .expect_err("combined single and batch forms are illegal");
        assert_eq!(error.to_string(), "use either task or tasks, not both");
        assert!(
            runtime.jobs(None).is_empty(),
            "combined forms must dispatch zero jobs"
        );
        assert!(
            runtime.list("Main").is_empty(),
            "combined forms must spawn zero children"
        );
    }

    /// User-reported failure 3: tasks=[] must be rejected before execution —
    /// schema-gate minItems first, executor fallback identical in policy.
    #[test]
    fn empty_tasks_batch_fails_schema_gate_and_executor_without_dispatch() {
        let prepared = fill_task_nulls(json!({"tasks": []})).expect("prepare");
        let error = task_schema()
            .validate(&prepared)
            .expect_err("empty batch must fail the schema gate");
        assert_eq!(error.to_string(), "$.tasks: must not have fewer than 1 items");
        let runtime = runtime(&["task"]);
        let error = parameters(json!({"tasks": []}))
            .into_items(&runtime)
            .expect_err("empty batch must fail the executor too");
        assert_eq!(error.to_string(), "tasks must not be empty");
        assert!(runtime.jobs(None).is_empty(), "empty batch must dispatch zero jobs");
        assert!(
            runtime.list("Main").is_empty(),
            "empty batch must spawn zero children"
        );
    }

    /// Legal single form: object and null outputSchema both dispatch exactly
    /// one job through schema → prepare → executor.
    #[tokio::test]
    async fn legal_single_task_accepts_object_and_null_output_schema() {
        for schema in [Some(json!({"type": "object"})), None, Some(Value::Null)] {
            let artifacts = tempfile::tempdir().expect("artifacts");
            let captured = Arc::new(parking_lot::Mutex::new(None));
            let release = Arc::new(tokio::sync::Notify::new());
            let runtime = execution_runtime(
                artifacts.path(),
                captured,
                release.clone(),
                json!({"ok": true}),
            );
            let tool = runtime.task_tool("Main", 0);
            let mut payload = json!({"task": "inspect source with acceptance criteria"});
            if let Some(schema) = schema {
                payload["outputSchema"] = schema;
            }
            let prepared = (tool.prepare_arguments.as_ref().expect("task prepare"))(payload)
                .expect("prepare single task");
            task_schema()
                .validate(&prepared)
                .expect("legal single form must pass the schema gate");
            let launch = (tool.execute)(task_context(prepared))
                .await
                .expect("launch single task");
            let spawns: Vec<super::super::TaskSpawn> =
                serde_json::from_value(launch.details).expect("authoritative spawn");
            assert_eq!(
                spawns.len(),
                1,
                "legal single form must dispatch exactly one spawn"
            );
            assert_eq!(runtime.jobs(None).len(), 1, "exactly one job created");
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    release.notify_waiters();
                    let jobs = runtime.jobs(None);
                    if jobs.len() == 1 && jobs.iter().all(|job| job.status == JobStatus::Completed)
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("single child settles exactly once");
            assert_eq!(runtime.jobs(None).len(), 1, "no extra dispatch after settle");
        }
    }

    /// Legal batch form: one job per item, ids preserved, no extra dispatch.
    #[tokio::test]
    async fn legal_batch_task_dispatches_exactly_once_per_item() {
        let artifacts = tempfile::tempdir().expect("artifacts");
        let captured = Arc::new(parking_lot::Mutex::new(None));
        let release = Arc::new(tokio::sync::Notify::new());
        let runtime = execution_runtime(
            artifacts.path(),
            captured,
            release.clone(),
            json!({"ok": true}),
        );
        let tool = runtime.task_tool("Main", 0);
        let payload = json!({
            "context": "shared background with acceptance criteria",
            "tasks": [
                {"name": "A", "task": "alpha assignment with acceptance criteria"},
                {"name": "B", "task": "beta assignment with acceptance criteria"},
            ],
        });
        let prepared = (tool.prepare_arguments.as_ref().expect("task prepare"))(payload)
            .expect("prepare batch");
        task_schema()
            .validate(&prepared)
            .expect("legal batch form must pass the schema gate");
        let launch = (tool.execute)(task_context(prepared))
            .await
            .expect("launch batch");
        let spawns: Vec<super::super::TaskSpawn> =
            serde_json::from_value(launch.details).expect("authoritative spawns");
        let ids: Vec<String> = spawns.iter().map(|spawn| spawn.agent_id.clone()).collect();
        assert_eq!(ids, ["A".to_owned(), "B".to_owned()]);
        assert_eq!(
            runtime.jobs(None).len(),
            2,
            "legal batch must dispatch exactly one job per item"
        );
        assert!(runtime.list("Main").len() == 2, "exactly two children");
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                release.notify_waiters();
                let jobs = runtime.jobs(None);
                if jobs.len() == 2 && jobs.iter().all(|job| job.status == JobStatus::Completed) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("both children settle exactly once");
        assert_eq!(runtime.jobs(None).len(), 2, "no extra dispatch after settle");
    }

    /// todoTaskId:"null" (the recurring string-null mistake) normalizes to
    /// JSON null in prepare — one rule feeding schema and executor alike.
    #[test]
    fn todo_task_id_string_null_normalizes_to_json_null_in_prepare() {
        for raw in [json!("null"), json!("  null  ")] {
            let prepared = fill_task_nulls(json!({"task": "inspect", "todoTaskId": raw}))
                .expect("prepare");
            assert_eq!(prepared["todoTaskId"], Value::Null, "{raw}");
            task_schema()
                .validate(&prepared)
                .expect("normalized null must stay schema-valid");
            let items = parameters(prepared).into_items(&runtime(&["task"])).expect("items");
            assert_eq!(items[0].todo_task_id, None, "executor must see no todo id");
        }
        let prepared = fill_task_nulls(json!({
            "context": "shared",
            "tasks": [
                {"task": "alpha work", "todoTaskId": "null"},
                {"task": "beta work", "todoTaskId": "todo-42"},
                {"task": "gamma work", "todoTaskId": Value::Null},
            ],
        }))
        .expect("prepare batch");
        assert_eq!(prepared["tasks"][0]["todoTaskId"], Value::Null);
        let items = parameters(prepared).into_items(&runtime(&["task"])).expect("items");
        assert_eq!(items[0].todo_task_id, None);
        assert_eq!(items[1].todo_task_id.as_deref(), Some("todo-42"));
        assert_eq!(items[2].todo_task_id, None);
    }

    /// Model-facing surface must never induce the rejected forms again.
    #[test]
    fn task_tool_never_induces_boolean_true_or_combined_forms() {
        let runtime = runtime(&["task"]);
        let tools = runtime.agent_tools("Main", 0);
        let task = tools.iter().find(|tool| tool.name == "task").expect("task tool");
        let schema = serde_json::to_value(task_schema()).expect("serialize task schema");
        fn collect_descriptions(value: &Value, out: &mut Vec<String>) {
            match value {
                Value::Object(map) => {
                    if let Some(Value::String(description)) = map.get("description") {
                        out.push(description.clone());
                    }
                    for child in map.values() {
                        collect_descriptions(child, out);
                    }
                }
                Value::Array(items) => {
                    for item in items {
                        collect_descriptions(item, out);
                    }
                }
                _ => {}
            }
        }
        let mut descriptions = vec![task.description.clone(), "task".to_owned()];
        collect_descriptions(&schema, &mut descriptions);
        for description in &descriptions {
            assert!(
                !description.contains("boolean true"),
                "descriptions must not mention boolean true: {description}"
            );
            assert!(
                !description.contains("true (accept"),
                "descriptions must not advertise accept-anything: {description}"
            );
        }
        assert!(
            task.description.contains("mutually exclusive")
                && task.description.contains("at least one item"),
            "tool description must state the exclusivity contract: {}",
            task.description
        );
        assert_eq!(schema["properties"]["tasks"]["minItems"], 1);
        assert_eq!(schema["properties"]["task"]["type"], json!(["string", "null"]));
    }
}

#[cfg(test)]
mod read_history_tests {
    use super::*;
    use crate::orchestration::runtime::register_test_agent;
    use crate::{AgentCatalog, AgentDefinition, AgentDefinitionSource, OrchestrationConfig};
    use pi_agent::ThinkingLevel;
    use std::fs;
    use std::path::Path;
    use std::sync::Arc;

    fn def(name: &str) -> AgentDefinition {
        AgentDefinition {
            name: name.to_owned(),
            description: format!("{name} description"),
            system_prompt: "p".to_owned(),
            tools: Some(Vec::new()),
            autoload_skills: Vec::new(),
            model: None,
            thinking_level: Some(ThinkingLevel::Off),
            max_turns: None,
            max_tool_calls: None,
            timeout_secs: None,
            disallowed_tools: Vec::new(),
            capability_ceiling: None,
            source: AgentDefinitionSource::Bundled,
            path: None,
            trusted: true,
            kind: crate::AgentDefinitionKind::Agent,
            personality: None,
            soft_budget: None,
            knowledge_roots: Vec::new(),
            user_proxy: false,
        }
    }

    fn runtime(root: &Path) -> OrchestrationRuntime {
        OrchestrationRuntime::new(
            OrchestrationConfig::new(AgentCatalog::from_agents(vec![def("task")]), root),
            Arc::new(|_| Box::pin(async { panic!("unused") })),
        )
        .expect("runtime")
    }

    fn session_token() -> String {
        ["s", "k-", "abcdefghijklmnop1234"].concat()
    }

    /// A settle-time history snapshot with a user message (carrying a
    /// secret), a tool result, and an assistant reply.
    fn session_transcript(_root: &Path) -> String {
        let user = pi_ai::Message::user_text(
            format!("first ask with {}\nsecond line", session_token()),
            0,
        );
        let tool = pi_ai::Message::ToolResult(pi_ai::ToolResultMessage {
            tool_call_id: "call_1".to_owned(),
            tool_name: "bash".to_owned(),
            content: vec![pi_ai::ContentBlock::text("raw output")],
            usage: None,
            details: None,
            added_tool_names: Vec::new(),
            is_error: false,
            timestamp: 0,
        });
        let mut assistant = pi_ai::AssistantMessage::pending(&pi_ai::Model::default());
        assistant.content = vec![
            pi_ai::ContentBlock::text("let me check"),
            pi_ai::ContentBlock::ToolCall(pi_ai::ToolCall {
                id: "call_2".to_owned(),
                name: "read".to_owned(),
                arguments: serde_json::json!({ "path": "seed.txt" }),
                thought_signature: None,
            }),
        ];
        serde_json::to_string(&vec![user, tool, pi_ai::Message::Assistant(assistant)])
            .expect("history snapshot")
    }

    fn text_of(result: &AgentToolResult) -> String {
        match result.content.first() {
            Some(pi_ai::ContentBlock::Text { text, .. }) => text.clone(),
            _ => String::new(),
        }
    }

    async fn run_hub(
        runtime: &OrchestrationRuntime,
        caller: &str,
        args: Value,
    ) -> Result<AgentToolResult> {
        let parameters: HubParameters = serde_json::from_value(args).expect("hub parameters");
        let (_ctrl, abort) = pi_agent::AbortController::new();
        std::mem::forget(_ctrl);
        execute_hub(runtime.clone(), caller.to_owned(), parameters, abort).await
    }

    #[tokio::test]
    async fn read_history_renders_bounded_transcript_without_raw_jsonl() {
        let root = tempfile::tempdir().expect("root");
        let runtime = runtime(root.path());
        let history_path = root.path().join("Sibling-job.history.json");
        fs::write(&history_path, session_transcript(root.path())).expect("history file");
        register_test_agent(&runtime, "Sibling", Some(history_path));

        let result = run_hub(
            &runtime,
            "Main",
            json!({
                "op": "read_history",
                "agentId": "Sibling",
            }),
        )
        .await
        .expect("read_history");
        let text = text_of(&result);
        assert!(text.contains("user: first ask with"), "{text}");
        assert!(text.contains("[tool: bash]"), "{text}");
        assert!(text.contains("assistant: let me check"), "{text}");
        // The assistant tool call must project its concrete target — a
        // generic-only history (`[tool: read]`/`[bash]` tags) is rejected.
        assert!(
            text.contains("assistant · read seed.txt"),
            "concrete tool action missing: {text}"
        );
        // Tool results carry the reliably-known outcome, never the output.
        assert!(text.contains("[tool: bash] · ok"), "{text}");
        // No raw JSONL leaks into the rendering.
        assert!(!text.contains("\"role\""), "no raw JSONL: {text}");
        assert!(!text.contains("toolCallId"), "no raw JSONL: {text}");
        assert!(!text.contains("\"type\""), "no raw JSONL: {text}");
        // Secrets are redacted.
        assert!(!text.contains(&session_token()), "secrets redacted: {text}");
        assert!(
            text.contains("[REDACTED]"),
            "redaction marker present: {text}"
        );
    }

    #[tokio::test]
    async fn read_history_parses_settle_time_snapshot_array() {
        let root = tempfile::tempdir().expect("root");
        let runtime = runtime(root.path());
        let history_path = root.path().join("Sibling-job.history.json");
        let mut assistant = pi_ai::AssistantMessage::pending(&pi_ai::Model::default());
        assistant.content = vec![pi_ai::ContentBlock::ToolCall(pi_ai::ToolCall {
            id: "call_2".to_owned(),
            name: "read".to_owned(),
            arguments: serde_json::json!({ "path": "seed.txt" }),
            thought_signature: None,
        })];
        let messages = vec![
            pi_ai::Message::user_text("snapshot ask", 0),
            pi_ai::Message::ToolResult(pi_ai::ToolResultMessage {
                tool_call_id: "call_1".to_owned(),
                tool_name: "read".to_owned(),
                content: vec![pi_ai::ContentBlock::text("output")],
                usage: None,
                details: None,
                added_tool_names: Vec::new(),
                is_error: false,
                timestamp: 0,
            }),
            pi_ai::Message::Assistant(assistant),
        ];
        fs::write(
            &history_path,
            serde_json::to_vec_pretty(&messages).expect("snapshot"),
        )
        .expect("history file");
        register_test_agent(&runtime, "Sibling", Some(history_path));

        let result = run_hub(
            &runtime,
            "Main",
            json!({
                "op": "read_history",
                "agentId": "Sibling",
            }),
        )
        .await
        .expect("read_history");
        let text = text_of(&result);
        assert!(text.contains("user: snapshot ask"), "{text}");
        assert!(text.contains("[tool: read]"), "{text}");
        assert!(
            text.contains("assistant · read seed.txt"),
            "snapshot history must project the concrete read target: {text}"
        );
    }

    #[tokio::test]
    async fn read_history_lines_bound_keeps_tail() {
        let root = tempfile::tempdir().expect("root");
        let runtime = runtime(root.path());
        let history_path = root.path().join("Sibling-job.history.json");
        let messages = (0..5u32)
            .map(|index| pi_ai::Message::user_text(format!("ask {index}"), 0))
            .collect::<Vec<_>>();
        fs::write(
            &history_path,
            serde_json::to_vec(&messages).expect("history snapshot"),
        )
        .expect("history file");
        register_test_agent(&runtime, "Sibling", Some(history_path));

        let result = run_hub(
            &runtime,
            "Main",
            json!({
                "op": "read_history",
                "agentId": "Sibling",
                "lines": 2,
            }),
        )
        .await
        .expect("read_history");
        let text = text_of(&result);
        assert!(text.contains("user: ask 3"), "{text}");
        assert!(text.contains("user: ask 4"), "{text}");
        assert!(!text.contains("ask 0"), "old entries dropped: {text}");
        assert!(!text.contains("ask 1"), "old entries dropped: {text}");
        assert_eq!(text.lines().count(), 2, "exactly two lines: {text}");
    }

    #[tokio::test]
    async fn read_history_unknown_agent_is_actionable() {
        let root = tempfile::tempdir().expect("root");
        let runtime = runtime(root.path());
        let error = run_hub(
            &runtime,
            "Main",
            json!({
                "op": "read_history",
                "agentId": "Ghost",
            }),
        )
        .await
        .expect_err("unknown agent");
        assert!(
            error.to_string().contains("unknown orchestration agent"),
            "{error:#}"
        );
    }

    #[tokio::test]
    async fn read_history_own_history_is_allowed() {
        let root = tempfile::tempdir().expect("root");
        let runtime = runtime(root.path());
        let history_path = root.path().join("Sibling-job.history.json");
        fs::write(&history_path, session_transcript(root.path())).expect("history file");
        register_test_agent(&runtime, "Sibling", Some(history_path));

        let result = run_hub(
            &runtime,
            "Sibling",
            json!({
                "op": "read_history",
                "agentId": "Sibling",
            }),
        )
        .await
        .expect("own history");
        let text = text_of(&result);
        assert!(text.contains("user: first ask with"), "{text}");
    }

    #[tokio::test]
    async fn read_history_rejects_path_traversal_agent_id() {
        let root = tempfile::tempdir().expect("root");
        let runtime = runtime(root.path());
        let error = run_hub(
            &runtime,
            "Main",
            json!({
                "op": "read_history",
                "agentId": "../etc/passwd",
            }),
        )
        .await
        .expect_err("traversal rejected");
        assert!(
            error
                .to_string()
                .contains("agent id must contain only ASCII"),
            "{error:#}"
        );
    }

    #[tokio::test]
    async fn read_history_rejects_out_of_range_lines() {
        let root = tempfile::tempdir().expect("root");
        let runtime = runtime(root.path());
        for lines in [0usize, 201] {
            let error = run_hub(
                &runtime,
                "Main",
                json!({
                    "op": "read_history",
                    "agentId": "Sibling",
                    "lines": lines,
                }),
            )
            .await
            .expect_err("lines bound");
            assert!(
                error.to_string().contains("lines must be between"),
                "{error:#}"
            );
        }
    }

    #[tokio::test]
    async fn read_history_requires_agent_id() {
        let root = tempfile::tempdir().expect("root");
        let runtime = runtime(root.path());
        let error = run_hub(
            &runtime,
            "Main",
            json!({
                "op": "read_history",
            }),
        )
        .await
        .expect_err("missing agentId");
        assert!(
            error.to_string().contains("agentId is required"),
            "{error:#}"
        );
    }

    #[tokio::test]
    async fn read_history_output_respects_lines_and_byte_bounds() {
        // 300 records: the line cap keeps the last 200, each label is capped
        // at 120 chars, and the total stays under the 32 KiB byte cap. No raw
        // JSON leaks and every rendered line fits a single line.
        let root = tempfile::tempdir().expect("root");
        let runtime = runtime(root.path());
        let history_path = root.path().join("Sibling-job.history.json");
        let messages = (0..300u32)
            .map(|index| pi_ai::Message::user_text(format!("ask {index} {}", "x".repeat(200)), 0))
            .collect::<Vec<_>>();
        fs::write(
            &history_path,
            serde_json::to_vec(&messages).expect("history snapshot"),
        )
        .expect("history file");
        register_test_agent(&runtime, "Sibling", Some(history_path));

        let result = run_hub(
            &runtime,
            "Main",
            json!({
                "op": "read_history",
                "agentId": "Sibling",
                "lines": 200,
            }),
        )
        .await
        .expect("read_history");
        let text = text_of(&result);
        let line_count = text.lines().count();
        assert!(
            line_count <= super::super::MAX_HISTORY_LINES,
            "line bound: {line_count}"
        );
        assert!(
            text.len() <= super::super::MAX_HISTORY_BYTES,
            "byte bound: rendered {} bytes, cap is {}",
            text.len(),
            super::super::MAX_HISTORY_BYTES
        );
        // The last 200 of 300 entries are kept: ask 100..299 present, ask 0..99 dropped.
        assert!(text.contains("user: ask 100 "), "tail kept: {text}");
        assert!(!text.contains("user: ask 99 "), "old entry dropped: {text}");
        assert!(!text.contains("user: ask 0 "), "oldest dropped: {text}");
        // No raw JSONL leaks despite 300 records.
        assert!(!text.contains("\"role\""), "no raw JSONL: {text}");
    }

    #[test]
    fn hub_schema_validate_matrix() {
        // Argument-validation matrix for all eight ops: positive cases the
        // schema accepts and negative cases it rejects (missing required
        // fields, empty values, incompatible cross-op fields, and the
        // `send` `await=true,to=all` combination). Replaces the prior flat
        // field-presence assertions: the oneOf-discriminated schema enforces
        // per-op contracts at the validation gate, ahead of `execute_hub`'s
        // fail-closed checks.
        let accept = |args: Value| {
            hub_schema()
                .validate(&args)
                .unwrap_or_else(|error| panic!("expected accept: {args} — {error}"));
        };
        let reject = |args: Value| {
            let result = hub_schema().validate(&args);
            assert!(result.is_err(), "expected reject but accepted: {args}");
        };

        // send — two arms split on `await`; `await=true,to=all` matches neither.
        accept(json!({"op":"send","to":"Alpha","message":"hi"}));
        accept(json!({"op":"send","to":"all","message":"hi"}));
        accept(json!({"op":"send","to":"Alpha","message":"hi","await":false}));
        accept(json!({"op":"send","to":"Alpha","message":"hi","replyTo":"m1"}));
        accept(json!({"op":"send","to":"Alpha","message":"hi","await":true,"timeoutMs":5000}));
        reject(json!({"op":"send","message":"hi"})); // missing to
        reject(json!({"op":"send","to":"Alpha"})); // missing message
        reject(json!({"op":"send","to":"","message":"hi"})); // empty to
        reject(json!({"op":"send","to":"Alpha","message":""})); // empty message
        reject(json!({"op":"send","to":"all","message":"hi","await":true})); // await+all
        reject(json!({"op":"send","to":"Alpha","message":"hi","bogus":1})); // unknown field
        reject(json!({"op":"send","to":"Alpha","message":"hi","await":"yes"})); // wrong await type

        // ask — single recipient (never all), reply implied (no await/replyTo).
        accept(json!({"op":"ask","to":"Alpha","message":"hi"}));
        accept(json!({"op":"ask","to":"Alpha","message":"hi","timeoutMs":5000}));
        reject(json!({"op":"ask","to":"all","message":"hi"})); // ask+all
        reject(json!({"op":"ask","to":"Alpha"})); // missing message
        reject(json!({"op":"ask","to":"Alpha","message":"hi","await":true})); // incompatible await
        reject(json!({"op":"ask","to":"Alpha","message":"hi","replyTo":"m1"})); // incompatible replyTo

        // wait — two arms: job-ids wait and message wait (mutually exclusive).
        accept(json!({"op":"wait"}));
        accept(json!({"op":"wait","from":"Main"}));
        accept(json!({"op":"wait","timeoutMs":1000}));
        accept(json!({"op":"wait","ids":["job1"]}));
        accept(json!({"op":"wait","ids":["job1"],"timeoutMs":1000}));
        reject(json!({"op":"wait","ids":[]})); // empty ids
        reject(json!({"op":"wait","ids":["job1"],"from":"Main"})); // ambiguous
        reject(json!({"op":"wait","bogus":1})); // unknown field

        // inbox
        accept(json!({"op":"inbox"}));
        accept(json!({"op":"inbox","peek":true}));
        reject(json!({"op":"inbox","ids":["job1"]})); // incompatible ids

        // list
        accept(json!({"op":"list"}));
        reject(json!({"op":"list","to":"Alpha"})); // extra field

        // jobs
        accept(json!({"op":"jobs"}));
        accept(json!({"op":"jobs","ids":["job1"]}));
        reject(json!({"op":"jobs","ids":[]})); // empty ids
        reject(json!({"op":"jobs","from":"Main"})); // incompatible from

        // cancel — ids required and non-empty.
        accept(json!({"op":"cancel","ids":["job1"]}));
        reject(json!({"op":"cancel"})); // missing ids
        reject(json!({"op":"cancel","ids":[]})); // empty ids
        reject(json!({"op":"cancel","ids":["job1"],"message":"hi"})); // incompatible message

        // read_history — agentId required and non-empty; lines in 1..=200.
        accept(json!({"op":"read_history","agentId":"Sibling"}));
        accept(json!({"op":"read_history","agentId":"Sibling","lines":50}));
        accept(json!({"op":"read_history","agentId":"Sibling","lines":1}));
        accept(json!({"op":"read_history","agentId":"Sibling","lines":200}));
        reject(json!({"op":"read_history"})); // missing agentId
        reject(json!({"op":"read_history","agentId":""})); // empty agentId
        reject(json!({"op":"read_history","agentId":"Sibling","lines":0})); // below min
        reject(json!({"op":"read_history","agentId":"Sibling","lines":201})); // above max
        reject(json!({"op":"read_history","agentId":"Sibling","to":"Alpha"})); // incompatible to

        // structural / cross-op
        reject(json!({"op":"bogus"})); // unknown op
        reject(json!({})); // missing op
    }

    #[test]
    fn hub_schema_advertises_union_fields_at_root() {
        let schema = hub_schema();
        let mut advertised = schema
            .properties
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>();
        advertised.sort_unstable();
        assert_eq!(
            advertised,
            vec![
                "agentId",
                "await",
                "from",
                "ids",
                "lines",
                "message",
                "op",
                "peek",
                "replyTo",
                "timeoutMs",
                "to",
            ],
        );
        assert_eq!(schema.required, vec!["op"]);
        assert_eq!(
            schema.one_of.len(),
            10,
            "local op contracts remain exclusive"
        );
    }

    #[test]
    fn user_ask_target_rewrites_to_single_user_proxy_persona() {
        let root = tempfile::tempdir().expect("root");
        let mut mentor = def("mentor");
        mentor.kind = crate::AgentDefinitionKind::Persona;
        mentor.user_proxy = true;
        let runtime = OrchestrationRuntime::new(
            OrchestrationConfig::new(
                AgentCatalog::from_agents(vec![def("task"), mentor]),
                root.path(),
            ),
            Arc::new(|_| Box::pin(async { unreachable!() })),
        )
        .expect("runtime");
        assert!(runtime.is_user_ask_target("user"));
        assert!(runtime.is_user_ask_target("Main"));
        assert!(!runtime.is_user_ask_target("task"));
        assert_eq!(
            runtime.user_proxy_agent().map(|agent| agent.name.as_str()),
            Some("mentor")
        );
    }

    #[test]
    fn two_user_proxy_personas_are_ambiguous() {
        let root = tempfile::tempdir().expect("root");
        let mut a = def("alpha");
        a.kind = crate::AgentDefinitionKind::Persona;
        a.user_proxy = true;
        let mut b = def("beta");
        b.kind = crate::AgentDefinitionKind::Persona;
        b.user_proxy = true;
        let runtime = OrchestrationRuntime::new(
            OrchestrationConfig::new(
                AgentCatalog::from_agents(vec![def("task"), a, b]),
                root.path(),
            ),
            Arc::new(|_| Box::pin(async { unreachable!() })),
        )
        .expect("runtime");
        assert!(runtime.user_proxy_agent().is_none());
    }
}
