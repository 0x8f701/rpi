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
                "Start one or more independent child coding-session jobs. Pass a single `task` for one spawn, or a `tasks` batch plus the REQUIRED shared `context` (background rendered into every child's system prompt as a CONTEXT section) to fan out several children in one call. Every `task` briefing MUST be complete and self-contained — state the objective, the concrete steps, and the acceptance criteria; one-liners or missing acceptance criteria are prohibited. Each batch item may set its own `agent` (which agent type runs it) and `outputSchema`/`schemaMode` (JSON Schema contract its delivered yield payload must satisfy). Returns immediately with stable job and agent ids; use hub jobs/wait/cancel to supervise completion. When a child owns a canonical todo DAG item, pass that item's stable id as todoTaskId; omit it for unrelated work. Available agents:\n{agents}"
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
            "Coordinate with Main and child peers. Supports send, wait, inbox, list, read_history, and cancellation/status for child task jobs. This tool does not supervise OS processes.",
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
        string_schema("The final deliverable text. This payload becomes the job's delivered output.", None),
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
                // OMP parity: `context` exists only in the batch shape; a
                // single-spawn call passing it is a caller error.
                if self
                    .context
                    .as_deref()
                    .is_some_and(|context| !context.trim().is_empty())
                {
                    bail!("context is only valid with a batch tasks[] call");
                }
                let (output_schema, schema_mode) = validate_output_contract(
                    self.output_schema,
                    self.schema_mode,
                    None,
                )?;
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
                    output_schema,
                    schema_mode,
                    ..TaskItem::default()
                }])
            }
            (None, Some(tasks)) => {
                if self.name.is_some() || self.agent.is_some() {
                    bail!("batch task calls must put name and agent inside each tasks[] item");
                }
                if tasks.is_empty() {
                    bail!("tasks must not be empty");
                }
                if tasks.len() > MAX_TASK_BATCH {
                    bail!("task batch exceeds maximum of {MAX_TASK_BATCH} items");
                }
                // OMP parity: `context` is REQUIRED shared background for a
                // batch, rendered into every child's system prompt as a
                // CONTEXT section.
                let shared = self
                    .context
                    .map(|context| context.trim().to_owned())
                    .filter(|context| !context.is_empty())
                    .ok_or_else(|| anyhow!("context is required for batch task calls"))?;
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
                        // Agent selection still sees the shared background
                        // (exact-mention routing and ranked selection behave
                        // as they did when context was concatenated), but the
                        // child's assignment is its own task only — the
                        // context lives in the system prompt's CONTEXT section.
                        let selection_text = format!("{shared}\n\n{}", item.task);
                        let agent =
                            runtime.resolve_task_agent(&selection_text, item.agent.as_deref())?;
                        Ok(TaskItem {
                            index,
                            id: item
                                .name
                                .filter(|name| !name.trim().is_empty())
                                .unwrap_or_else(|| runtime.generated_agent_id(index)),
                            agent,
                            assignment: item.task,
                            todo_task_id: item.todo_task_id,
                            context: Some(shared.clone()),
                            output_schema,
                            schema_mode,
                        })
                    })
                    .collect()
            }
            (Some(_), Some(_)) => bail!("use either task or tasks, not both"),
            (None, None) => bail!("task requires either task or tasks"),
        }
    }
}

/// Validate a per-item (or single-spawn) `outputSchema`/`schemaMode` pair.
/// `schemaMode` must be `permissive` or `strict` when present; `outputSchema`
/// must be a JSON Schema object (or `true`, the accept-everything schema) and
/// is pre-flighted through the enforceable subset so malformed schemas fail
/// the call actionably instead of surfacing at child settle time.
fn validate_output_contract(
    output_schema: Option<Value>,
    schema_mode: Option<String>,
    index: Option<usize>,
) -> Result<(Option<Value>, Option<String>)> {
    let where_ = index.map_or_else(|| "task".to_owned(), |index| format!("tasks[{index}]"));
    let mode = match schema_mode.as_deref() {
        None | Some("permissive") => "permissive",
        Some("strict") => "strict",
        Some(other) => bail!("{where_}.schemaMode must be \"permissive\" or \"strict\", got {other:?}"),
    };
    let schema = match output_schema {
        None | Some(Value::Null) => None,
        Some(schema @ (Value::Object(_) | Value::Bool(true))) => {
            // Fail fast on malformed schemas; the parsed form is rebuilt at
            // settle time (TaskItem carries the raw value).
            parse_output_schema(&schema).map_err(|error| {
                anyhow!("{where_}.outputSchema is not a supported JSON Schema: {error:#}")
            })?;
            Some(schema)
        }
        Some(Value::Bool(false)) => bail!("{where_}.outputSchema false is not supported"),
        Some(other) => bail!(
            "{where_}.outputSchema must be a JSON Schema object, got {}",
            json_type_name(&other)
        ),
    };
    Ok((schema, mode.eq("strict").then(|| mode.to_owned())))
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
            let receipts = runtime.send(&caller_id, &to, &message, parameters.reply_to);
            let mut lines = receipts
                .iter()
                .map(|receipt| {
                    let label = match receipt.outcome {
                        DeliveryOutcome::Queued => "queued",
                        DeliveryOutcome::Woken => "woken",
                        DeliveryOutcome::Revived => "revived",
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
            if parameters.await_reply
                && receipts.iter().any(|receipt| receipt.error.is_none())
            {
                // Prefer the canonical agent id from a successful receipt so
                // await-from tracks job-UUID sends after resolution.
                let await_from = receipts
                    .iter()
                    .find(|receipt| receipt.error.is_none())
                    .map(|receipt| receipt.to.as_str())
                    .unwrap_or(to.as_str());
                lines.push(String::new());
                let reply = runtime
                    .wait_message(
                        &caller_id,
                        Some(await_from),
                        Some(Duration::from_millis(
                            parameters.timeout_ms.unwrap_or(DEFAULT_WAIT_TIMEOUT_MS),
                        )),
                        Some(abort),
                    )
                    .await?;
                match &reply {
                    Some(reply) => lines.push(format!("Reply from {}: {}", reply.from, reply.body)),
                    None => lines.push(format!("No reply from {await_from} before timeout.")),
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
    jobs
        .iter()
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
            "JSON Schema contract for this child's delivered yield payload: the child must deliver a single JSON value that validates against it. May also be the boolean true (accept anything)".to_owned(),
        ),
        ..Schema::default()
    }
}

fn schema_mode_field() -> Schema {
    nullable(string_schema(
        "Validation mode for outputSchema: \"permissive\" (report-only, default) or \"strict\" (a validation failure surfaces as a job error)",
        None,
    ))
}

fn task_schema() -> Schema {
    let item = strict_object(vec![
        ("name", nullable(string_schema("Stable child id", None)), true),
        ("agent", nullable(string_schema("Agent definition name", None)), true),
        ("todoTaskId", nullable(string_schema("Stable canonical todo task id owned by this child; null for unrelated work", None)), true),
        ("task", string_schema("Complete, self-contained assignment for this child, including acceptance criteria; never a one-liner", Some(1)), true),
        ("outputSchema", nullable(output_schema_field()), true),
        ("schemaMode", schema_mode_field(), true),
    ]);
    strict_object(vec![
        ("name", nullable(string_schema("Stable child id", None)), true),
        ("agent", nullable(string_schema("Agent definition name", None)), true),
        ("task", nullable(string_schema("Complete, self-contained single-child assignment, including acceptance criteria; never a one-liner", Some(1))), true),
        ("todoTaskId", nullable(string_schema("Stable canonical todo task id owned by the single child; null for unrelated work", None)), true),
        ("context", nullable(string_schema("REQUIRED shared background for a batch tasks[] call, rendered into every child's system prompt as a CONTEXT section; invalid with a single task", None)), true),
        (
            "tasks",
            nullable(Schema {
                schema_type: Some(Value::String("array".to_owned())),
                description: Some("Independent child assignments (batch). Each item needs a complete self-contained task briefing with acceptance criteria; context is required alongside".to_owned()),
                items: Some(Box::new(item)), min_items: Some(1), max_items: Some(MAX_TASK_BATCH),
                ..Schema::default()
            }),
            true,
        ),
        ("outputSchema", nullable(output_schema_field()), true),
        ("schemaMode", schema_mode_field(), true),
    ])
}

fn nullable(mut schema: Schema) -> Schema { schema.nullable = true; schema }

fn fill_task_nulls(mut arguments: Value) -> Result<Value> {
    let object = arguments.as_object_mut().ok_or_else(|| anyhow!("task arguments must be an object"))?;
    for key in ["name", "agent", "task", "todoTaskId", "context", "tasks", "outputSchema", "schemaMode"] {
        object.entry(key).or_insert(Value::Null);
    }
    if let Some(tasks) = object.get_mut("tasks").and_then(Value::as_array_mut) {
        for item in tasks {
            let item = item.as_object_mut().ok_or_else(|| anyhow!("each task entry must be an object"))?;
            for key in ["name", "agent", "todoTaskId", "outputSchema", "schemaMode"] { item.entry(key).or_insert(Value::Null); }
        }
    }
    Ok(arguments)
}

fn hub_schema() -> Schema {
    strict_object(vec![
        (
            "op",
            string_schema(
                "Operation: send, wait, inbox, list, jobs, cancel, or read_history",
                Some(1),
            )
            .with_enum(["send", "wait", "inbox", "list", "jobs", "cancel", "read_history"]),
            true,
        ),
        ("to", string_schema("Recipient id or all", None), false),
        ("message", string_schema("Message body", None), false),
        ("replyTo", string_schema("Message id being answered", None), false),
        (
            "await",
            Schema {
                schema_type: Some(Value::String("boolean".to_owned())),
                ..Schema::default()
            },
            false,
        ),
        ("from", string_schema("Only wait for this message sender", None), false),
        (
            "timeoutMs",
            Schema {
                schema_type: Some(Value::String("integer".to_owned())),
                minimum: Some(0.0),
                ..Schema::default()
            },
            false,
        ),
        (
            "peek",
            Schema {
                schema_type: Some(Value::String("boolean".to_owned())),
                ..Schema::default()
            },
            false,
        ),
        (
            "ids",
            Schema {
                schema_type: Some(Value::String("array".to_owned())),
                items: Some(Box::new(string_schema("Job id or child agent id", Some(1)))),
                min_items: Some(1),
                ..Schema::default()
            },
            false,
        ),
        (
            "agentId",
            string_schema("Agent id whose session transcript to read (read_history)", None),
            false,
        ),
        (
            "lines",
            Schema {
                schema_type: Some(Value::String("integer".to_owned())),
                description: Some(
                    "Number of most recent transcript entries to render (read_history, 1..=200, default 50)"
                        .to_owned(),
                ),
                minimum: Some(1.0),
                maximum: Some(super::MAX_HISTORY_LINES as f64),
                ..Schema::default()
            },
            false,
        ),
    ])
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

/// Parse an invocation `outputSchema` (a raw JSON Schema value) into the
/// enforceable [`Schema`] subset used by the payload validator. `true` is the
/// accept-everything JSON Schema; `false` (reject-everything) and `$ref`
/// (unresolvable without a schema registry) fail actionably. Unknown keywords
/// round-trip through `Schema::extra` and are skipped by validation, matching
/// the JSON Schema convention that unknown keywords are annotations.
pub(crate) fn parse_output_schema(schema: &Value) -> Result<Schema> {
    match schema {
        Value::Bool(true) => Ok(Schema::default()),
        Value::Bool(false) => bail!("outputSchema false is not supported"),
        Value::Object(object) if object.contains_key("$ref") => {
            bail!("outputSchema $ref is not supported")
        }
        Value::Object(_) => {
            serde_json::from_value(schema.clone()).map_err(|error| anyhow!("{error}"))
        }
        other => bail!(
            "outputSchema must be a JSON Schema object, got {}",
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
            Some(format!("delivered payload is not valid JSON: {parse_error}")),
        ),
        Ok(data) => match parse_output_schema(schema_value).and_then(|schema| schema.validate(&data))
        {
            Ok(validated) => (true, Some(validated), None),
            Err(validation_error) => (
                false,
                Some(data),
                Some(format!(
                    "delivered payload failed outputSchema validation: {validation_error:#}"
                )),
            ),
        },
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
    use crate::{AgentCatalog, AgentDefinition, AgentDefinitionSource, AgentRuntimeSettings, OrchestrationConfig};
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
        let runtime = OrchestrationRuntime::new(
            config,
            Arc::new(|_| Box::pin(async { panic!("unused") })),
        )
        .expect("runtime");
        let tools = runtime.agent_tools("Main", 0);
        let task = tools.iter().find(|tool| tool.name == "task").expect("task tool");
        assert!(task.description.contains("task —"));
        assert!(!task.description.contains("reviewer —"), "{}", task.description);
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
            tools.iter().find(|tool| tool.name == "task").unwrap().capability,
            ToolCapability::Exec
        );
        assert_eq!(
            tools.iter().find(|tool| tool.name == "hub").unwrap().capability,
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
        for name in properties.keys() { assert!(required.iter().any(|item| item == name)); }
        let prepared = fill_task_nulls(json!({"task":"inspect source"})).expect("prepare task");
        assert!(task_schema().validate(&prepared).is_ok());
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
    fn single_spawn_rejects_context_omp_parity() {
        let error = parameters(json!({"task": "inspect", "context": "shared"}))
            .into_items(&runtime(&["task"]))
            .expect_err("context must be rejected with a single task");
        assert!(
            error.to_string().contains("context is only valid with a batch"),
            "{error:#}"
        );
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
            items[0].output_schema.as_ref().and_then(|s| s.get("type")).and_then(Value::as_str),
            Some("object")
        );
        assert_eq!(items[0].schema_mode.as_deref(), Some("strict"));
    }

    #[test]
    fn batch_requires_shared_context() {
        let error = parameters(json!({"tasks": [{"task": "one"}]}))
            .into_items(&runtime(&["task"]))
            .expect_err("batch without context must fail");
        assert!(
            error.to_string().contains("context is required for batch"),
            "{error:#}"
        );
        // Whitespace-only context is equally rejected.
        let error = parameters(json!({"context": "   ", "tasks": [{"task": "one"}]}))
            .into_items(&runtime(&["task"]))
            .expect_err("blank context must fail");
        assert!(
            error.to_string().contains("context is required for batch"),
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
        // Top-level name/agent stay rejected in batch shape (OMP: per item).
        let error = parameters(json!({"agent": "reviewer", "context": "c", "tasks": [{"task": "t"}]}))
            .into_items(&runtime(&["task", "reviewer"]))
            .expect_err("top-level agent must be rejected in batch shape");
        assert!(error.to_string().contains("inside each tasks[] item"), "{error:#}");
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
            items[0].output_schema.as_ref().and_then(|s| s.get("type")).and_then(Value::as_str),
            Some("object")
        );
        assert_eq!(items[0].schema_mode.as_deref(), Some("strict"));
        assert_eq!(items[1].output_schema, None);
        assert_eq!(items[1].schema_mode, None);
        // An omitted schemaMode with a schema is fine (defaults permissive).
        assert_eq!(items[1].schema_mode, None);
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
        for bad in [json!("not-a-schema"), json!(false), json!({"$ref": "#/definitions/x"}), json!(42)] {
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
        assert!(task_schema().validate(&prepared).is_ok(), "validated batch args");
    }

    #[test]
    fn delivered_payload_is_validated_against_the_schema() {
        let schema = json!({
            "type": "object",
            "properties": {"ok": {"type": "boolean"}},
            "required": ["ok"],
        });
        // Conforming JSON payload → valid, parsed data surfaced.
        let report = validate_delivered_output(
            r#"{"ok": true}"#,
            Some(&schema),
            Some("strict"),
            None,
        )
        .expect("report");
        assert!(report.valid, "conforming payload must validate");
        assert_eq!(report.schema_source, "task");
        assert_eq!(report.schema_mode, "strict");
        assert_eq!(report.data, Some(json!({"ok": true})));
        assert_eq!(report.error, None);

        // Non-conforming JSON → invalid with a path-annotated error; the
        // parsed data is still surfaced for inspection.
        let report = validate_delivered_output(
            r#"{"ok": "nope"}"#,
            Some(&schema),
            None,
            None,
        )
        .expect("report");
        assert!(!report.valid, "non-conforming payload must fail");
        assert_eq!(report.schema_mode, "permissive", "absent mode defaults to permissive");
        assert_eq!(report.data, Some(json!({"ok": "nope"})));
        assert!(report.error.as_deref().is_some_and(|error| error.contains("outputSchema")));

        // Non-JSON payload → invalid with a parse error and no data.
        let report = validate_delivered_output("not json at all", Some(&schema), None, None)
            .expect("report");
        assert!(!report.valid);
        assert_eq!(report.data, None);
        assert!(report.error.as_deref().is_some_and(|error| error.contains("not valid JSON")));

        // No contract → no report; an errored run → no report either.
        assert!(validate_delivered_output("anything", None, None, None).is_none());
        assert!(validate_delivered_output("anything", Some(&schema), None, Some("run failed")).is_none());

        // The accept-everything `true` schema validates any JSON payload.
        let report = validate_delivered_output("42", Some(&json!(true)), None, None)
            .expect("report");
        assert!(report.valid);
        assert_eq!(report.data, Some(json!(42)));
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

    /// A durable-child-JSONL-style session transcript with a header, a user
    /// message (carrying a secret), a tool result, and an assistant reply.
    fn session_transcript(root: &Path) -> String {
        let header = json!({
            "type": "session",
            "version": crate::session_store::CURRENT_SESSION_VERSION,
            "id": "child-session",
            "timestamp": "2026-01-01T00:00:00.000Z",
            "cwd": root,
        });
        let user = serde_json::to_value(pi_ai::Message::user_text(
            format!("first ask with {}\nsecond line", session_token()),
            0,
        ))
        .expect("user message");
        let tool = serde_json::to_value(pi_ai::Message::ToolResult(pi_ai::ToolResultMessage {
            tool_call_id: "call_1".to_owned(),
            tool_name: "bash".to_owned(),
            content: vec![pi_ai::ContentBlock::text("raw output")],
            usage: None,
            details: None,
            added_tool_names: Vec::new(),
            is_error: false,
            timestamp: 0,
        }))
        .expect("tool message");
        let mut assistant = pi_ai::AssistantMessage::pending(&pi_ai::Model::default());
        assistant.content = vec![pi_ai::ContentBlock::text("let me check")];
        let assistant = serde_json::to_value(pi_ai::Message::Assistant(assistant))
            .expect("assistant message");
        let records = [
            json!({
                "type": "message",
                "id": "m1",
                "parentId": null,
                "timestamp": "2026-01-01T00:00:01.000Z",
                "message": user,
            }),
            json!({
                "type": "message",
                "id": "m2",
                "parentId": "m1",
                "timestamp": "2026-01-01T00:00:02.000Z",
                "message": tool,
            }),
            json!({
                "type": "message",
                "id": "m3",
                "parentId": "m2",
                "timestamp": "2026-01-01T00:00:03.000Z",
                "message": assistant,
            }),
        ];
        let mut lines = vec![serde_json::to_string(&header).expect("header")];
        lines.extend(
            records
                .iter()
                .map(|record| serde_json::to_string(record).expect("record")),
        );
        lines.join("\n")
    }

    fn text_of(result: &AgentToolResult) -> String {
        match result.content.first() {
            Some(pi_ai::ContentBlock::Text { text, .. }) => text.clone(),
            _ => String::new(),
        }
    }

    async fn run_hub(runtime: &OrchestrationRuntime, caller: &str, args: Value) -> Result<AgentToolResult> {
        let parameters: HubParameters =
            serde_json::from_value(args).expect("hub parameters");
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

        let result = run_hub(&runtime, "Main", json!({
            "op": "read_history",
            "agentId": "Sibling",
        }))
        .await
        .expect("read_history");
        let text = text_of(&result);
        assert!(text.contains("user: first ask with"), "{text}");
        assert!(text.contains("[tool: bash]"), "{text}");
        assert!(text.contains("assistant: let me check"), "{text}");
        // No raw JSONL leaks into the rendering.
        assert!(!text.contains("\"role\""), "no raw JSONL: {text}");
        assert!(!text.contains("toolCallId"), "no raw JSONL: {text}");
        assert!(!text.contains("\"type\""), "no raw JSONL: {text}");
        // Secrets are redacted.
        assert!(!text.contains(&session_token()), "secrets redacted: {text}");
        assert!(text.contains("[REDACTED]"), "redaction marker present: {text}");
        // Tool results render as just the tag (no output preview).
        assert!(!text.contains("raw output"), "tool output not previewed: {text}");
    }

    #[tokio::test]
    async fn read_history_parses_settle_time_snapshot_array() {
        let root = tempfile::tempdir().expect("root");
        let runtime = runtime(root.path());
        let history_path = root.path().join("Sibling-job.history.json");
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
        ];
        fs::write(&history_path, serde_json::to_vec_pretty(&messages).expect("snapshot"))
            .expect("history file");
        register_test_agent(&runtime, "Sibling", Some(history_path));

        let result = run_hub(&runtime, "Main", json!({
            "op": "read_history",
            "agentId": "Sibling",
        }))
        .await
        .expect("read_history");
        let text = text_of(&result);
        assert!(text.contains("user: snapshot ask"), "{text}");
        assert!(text.contains("[tool: read]"), "{text}");
    }

    #[tokio::test]
    async fn read_history_lines_bound_keeps_tail() {
        let root = tempfile::tempdir().expect("root");
        let runtime = runtime(root.path());
        let history_path = root.path().join("Sibling-job.history.json");
        let header = json!({
            "type": "session",
            "version": crate::session_store::CURRENT_SESSION_VERSION,
            "id": "child-session",
            "timestamp": "2026-01-01T00:00:00.000Z",
            "cwd": root.path(),
        });
        let mut lines = vec![serde_json::to_string(&header).expect("header")];
        for index in 0..5u32 {
            let record = json!({
                "type": "message",
                "id": format!("m{index}"),
                "parentId": if index == 0 { Value::Null } else { Value::String(format!("m{}", index - 1)) },
                "timestamp": format!("2026-01-01T00:00:0{index}.000Z"),
                "message": serde_json::to_value(pi_ai::Message::user_text(format!("ask {index}"), 0))
                    .expect("message"),
            });
            lines.push(serde_json::to_string(&record).expect("record"));
        }
        fs::write(&history_path, lines.join("\n")).expect("history file");
        register_test_agent(&runtime, "Sibling", Some(history_path));

        let result = run_hub(&runtime, "Main", json!({
            "op": "read_history",
            "agentId": "Sibling",
            "lines": 2,
        }))
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
        let error = run_hub(&runtime, "Main", json!({
            "op": "read_history",
            "agentId": "Ghost",
        }))
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

        let result = run_hub(&runtime, "Sibling", json!({
            "op": "read_history",
            "agentId": "Sibling",
        }))
        .await
        .expect("own history");
        let text = text_of(&result);
        assert!(text.contains("user: first ask with"), "{text}");
    }

    #[tokio::test]
    async fn read_history_rejects_path_traversal_agent_id() {
        let root = tempfile::tempdir().expect("root");
        let runtime = runtime(root.path());
        let error = run_hub(&runtime, "Main", json!({
            "op": "read_history",
            "agentId": "../etc/passwd",
        }))
        .await
        .expect_err("traversal rejected");
        assert!(
            error.to_string().contains("agent id must contain only ASCII"),
            "{error:#}"
        );
    }

    #[tokio::test]
    async fn read_history_rejects_out_of_range_lines() {
        let root = tempfile::tempdir().expect("root");
        let runtime = runtime(root.path());
        for lines in [0usize, 201] {
            let error = run_hub(&runtime, "Main", json!({
                "op": "read_history",
                "agentId": "Sibling",
                "lines": lines,
            }))
            .await
            .expect_err("lines bound");
            assert!(error.to_string().contains("lines must be between"), "{error:#}");
        }
    }

    #[tokio::test]
    async fn read_history_requires_agent_id() {
        let root = tempfile::tempdir().expect("root");
        let runtime = runtime(root.path());
        let error = run_hub(&runtime, "Main", json!({
            "op": "read_history",
        }))
        .await
        .expect_err("missing agentId");
        assert!(error.to_string().contains("agentId is required"), "{error:#}");
    }

    #[tokio::test]
    async fn read_history_output_respects_lines_and_byte_bounds() {
        // 300 records: the line cap keeps the last 200, each label is capped
        // at 120 chars, and the total stays under the 32 KiB byte cap. No raw
        // JSON leaks and every rendered line fits a single line.
        let root = tempfile::tempdir().expect("root");
        let runtime = runtime(root.path());
        let history_path = root.path().join("Sibling-job.history.json");
        let header = json!({
            "type": "session",
            "version": crate::session_store::CURRENT_SESSION_VERSION,
            "id": "child-session",
            "timestamp": "2026-01-01T00:00:00.000Z",
            "cwd": root.path(),
        });
        let mut lines = vec![serde_json::to_string(&header).expect("header")];
        for index in 0..300u32 {
            let record = json!({
                "type": "message",
                "id": format!("m{index}"),
                "parentId": if index == 0 { Value::Null } else { Value::String(format!("m{}", index - 1)) },
                "timestamp": "2026-01-01T00:00:00.000Z",
                "message": serde_json::to_value(pi_ai::Message::user_text(
                    format!("ask {index} {}", "x".repeat(200)),
                    0,
                ))
                .expect("message"),
            });
            lines.push(serde_json::to_string(&record).expect("record"));
        }
        fs::write(&history_path, lines.join("\n")).expect("history file");
        register_test_agent(&runtime, "Sibling", Some(history_path));

        let result = run_hub(&runtime, "Main", json!({
            "op": "read_history",
            "agentId": "Sibling",
            "lines": 200,
        }))
        .await
        .expect("read_history");
        let text = text_of(&result);
        let line_count = text.lines().count();
        assert!(line_count <= super::super::MAX_HISTORY_LINES, "line bound: {line_count}");
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
    fn hub_schema_includes_read_history_op_and_args() {
        let schema = serde_json::to_value(hub_schema()).expect("hub schema");
        let op_enum = schema["properties"]["op"]["enum"]
            .as_array()
            .expect("op enum");
        let ops: Vec<&str> = op_enum.iter().filter_map(Value::as_str).collect();
        assert!(ops.contains(&"read_history"), "op enum: {ops:?}");
        assert!(schema["properties"].get("agentId").is_some(), "agentId field");
        let lines = &schema["properties"]["lines"];
        assert_eq!(lines["type"], "integer", "lines type");
        assert_eq!(lines["minimum"].as_f64(), Some(1.0), "lines minimum");
        assert_eq!(
            lines["maximum"].as_f64(),
            Some(super::super::MAX_HISTORY_LINES as f64),
            "lines maximum"
        );
    }
}
