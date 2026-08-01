use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use pi_agent::{AgentTool, AgentToolResult, ToolExecutionMode};
use pi_ai::Schema;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{AgentStatus, DeliveryOutcome, JobSnapshot, JobStatus, OrchestrationRuntime, TaskItem};

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
                "Start one or more independent child coding-session jobs. Returns immediately with stable job and agent ids; use hub jobs/wait/cancel to supervise completion. Available agents:\n{agents}"
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
        .with_execution_mode(ToolExecutionMode::Sequential)
    }

    fn hub_tool(&self, caller_id: &str) -> AgentTool {
        let runtime = self.clone();
        let caller_id = caller_id.to_owned();
        AgentTool::new(
            "hub",
            "Coordinate with Main and child peers. Supports send, wait, inbox, list, and cancellation/status for child task jobs. This tool does not supervise OS processes.",
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
        .with_execution_mode(ToolExecutionMode::Sequential)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskParameters {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    task: Option<String>,
    #[serde(default)]
    context: Option<String>,
    #[serde(default)]
    tasks: Option<Vec<TaskParametersItem>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskParametersItem {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    agent: Option<String>,
    task: String,
}

impl TaskParameters {
    fn into_items(self, runtime: &OrchestrationRuntime) -> Result<Vec<TaskItem>> {
        match (self.task, self.tasks) {
            (Some(task), None) => {
                if task.trim().is_empty() {
                    bail!("task must not be empty");
                }
                let agent = runtime.select_agent(&task, self.agent.as_deref());
                runtime.ensure_agent_enabled(&agent)?;
                Ok(vec![TaskItem {
                    index: 0,
                    id: self
                        .name
                        .filter(|name| !name.trim().is_empty())
                        .unwrap_or_else(|| runtime.generated_agent_id(0)),
                    agent,
                    assignment: task,
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
                let shared = self.context.unwrap_or_default();
                tasks
                    .into_iter()
                    .enumerate()
                    .map(|(index, item)| {
                        if item.task.trim().is_empty() {
                            bail!("tasks[{index}].task must not be empty");
                        }
                        let assignment = if shared.trim().is_empty() {
                            item.task
                        } else {
                            format!("{shared}\n\n{}", item.task)
                        };
                        let agent = runtime.select_agent(&assignment, item.agent.as_deref());
                        runtime.ensure_agent_enabled(&agent)?;
                        Ok(TaskItem {
                            index,
                            id: item
                                .name
                                .filter(|name| !name.trim().is_empty())
                                .unwrap_or_else(|| runtime.generated_agent_id(index)),
                            agent,
                            assignment,
                        })
                    })
                    .collect()
            }
            (Some(_), Some(_)) => bail!("use either task or tasks, not both"),
            (None, None) => bail!("task requires either task or tasks"),
        }
    }
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
                    match (&receipt.error, receipt.outcome) {
                        (Some(error), DeliveryOutcome::Failed) => {
                            format!("- {}: failed — {error}", receipt.to)
                        }
                        (Some(error), _) => format!("- {}: {} — {error}", receipt.to, label),
                        (None, _) => format!("- {}: {}", receipt.to, label),
                    }
                })
                .collect::<Vec<_>>();
            if parameters.await_reply
                && receipts.iter().any(|receipt| receipt.error.is_none())
            {
                lines.push(String::new());
                let reply = runtime
                    .wait_message(
                        &caller_id,
                        Some(&to),
                        Some(Duration::from_millis(
                            parameters.timeout_ms.unwrap_or(DEFAULT_WAIT_TIMEOUT_MS),
                        )),
                        Some(abort),
                    )
                    .await?;
                if let Some(reply) = reply {
                    lines.push(format!("Reply from {}: {}", reply.from, reply.body));
                } else {
                    lines.push(format!("No reply from {to} before timeout."));
                }
            }
            Ok(result(
                if lines.is_empty() {
                    "No recipients.".to_owned()
                } else {
                    lines.join("\n")
                },
                json!({ "op": "send", "receipts": receipts }),
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
            let messages = runtime.inbox(&caller_id, parameters.peek);
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
            let text = if peers.is_empty() {
                "No other agents.".to_owned()
            } else {
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
                            "- {} [{}; unread {}; parent {}]",
                            peer.id,
                            status,
                            peer.unread,
                            peer.parent_id.as_deref().unwrap_or("none")
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            };
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
            let cancelled = runtime.cancel_jobs(&ids);
            Ok(result(
                if cancelled.is_empty() {
                    "No running child tasks matched.".to_owned()
                } else {
                    format!("Cancelled: {}", cancelled.join(", "))
                },
                json!({ "op": "cancel", "cancelled": cancelled }),
            ))
        }
        operation => bail!("unsupported hub operation {operation:?}"),
    }
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

fn task_schema() -> Schema {
    let item = strict_object(vec![
        ("name", string_schema("Stable child id", None), false),
        ("agent", string_schema("Agent definition name", None), false),
        ("task", string_schema("Assignment for this child", Some(1)), true),
    ]);
    let single = strict_object(vec![
        ("name", string_schema("Stable child id", None), false),
        ("agent", string_schema("Agent definition name", None), false),
        ("task", string_schema("Assignment for this child", Some(1)), true),
    ]);
    let batch = strict_object(vec![
        (
            "context",
            string_schema("Shared context prepended to every child assignment", None),
            false,
        ),
        (
            "tasks",
            Schema {
                schema_type: Some(Value::String("array".to_owned())),
                description: Some("Independent child assignments".to_owned()),
                items: Some(Box::new(item)),
                min_items: Some(1),
                max_items: Some(MAX_TASK_BATCH),
                ..Schema::default()
            },
            true,
        ),
    ]);
    Schema {
        one_of: vec![single, batch],
        ..Schema::default()
    }
}

fn hub_schema() -> Schema {
    strict_object(vec![
        (
            "op",
            string_schema(
                "Operation: send, wait, inbox, list, jobs, or cancel",
                Some(1),
            )
            .with_enum(["send", "wait", "inbox", "list", "jobs", "cancel"]),
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
            source: AgentDefinitionSource::Bundled,
            path: None,
            trusted: true,
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
}
