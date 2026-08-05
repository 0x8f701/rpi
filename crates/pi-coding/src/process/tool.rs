use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use base64::Engine as _;
use pi_agent::{AbortSignal, AgentTool, AgentToolResult, ToolCapability};
use pi_ai::Schema;
use serde_json::{Value, json};

use super::{
    ProcessId, ProcessKey, ProcessManager, ProcessOwnerId, ProcessSignal, ProcessSpawnSpec,
    ProcessTerminalSize,
};

pub fn process_tool(
    cwd: &Path,
    manager: ProcessManager,
    owner_id: ProcessOwnerId,
) -> AgentTool {
    let cwd = cwd.to_path_buf();
    AgentTool::new(
        "process",
        "Start and supervise long-running argv-based processes owned by the current Application. Every start returns a stable id listed by /ps and supports PTY input, bounded cursor logs, signals, stop, wait, and describe. Commands are never shell-interpolated.",
        process_schema(),
        move |context| {
            let manager = manager.clone();
            let owner_id = owner_id.clone();
            let cwd = cwd.clone();
            async move { execute_process_tool(&manager, &owner_id, &cwd, context.arguments, context.abort).await }
        },
    )
    .with_capability(ToolCapability::Exec)
    .with_prompt_guidelines(vec![
        "Use process start for servers, watchers, long-running, or interactive commands; use bash only for finite foreground commands.".to_owned(),
        "Never launch long-lived work with nohup, setsid, disown, or shell '&'. A supervised process must have a stable id visible in /ps with logs, signal, stop, and wait controls.".to_owned(),
        "Pass an argv array to process start; shell syntax is not interpreted.".to_owned(),
    ])
}

async fn execute_process_tool(
    manager: &ProcessManager,
    owner_id: &ProcessOwnerId,
    cwd: &Path,
    args: Value,
    abort: AbortSignal,
) -> Result<AgentToolResult> {
    if abort.is_aborted() {
        bail!("Operation aborted");
    }
    let op = args.get("op").and_then(Value::as_str).unwrap_or_default();
    let details = match op {
        "start" => {
            let argv = args
                .get("argv")
                .and_then(Value::as_array)
                .ok_or_else(|| anyhow!("process start requires argv"))?
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| anyhow!("argv entries must be strings"))
                })
                .collect::<Result<Vec<_>>>()?;
            let process_cwd = args
                .get("cwd")
                .and_then(Value::as_str)
                .map_or_else(|| cwd.to_path_buf(), PathBuf::from);
            let env = parse_env(args.get("env"))?;
            let tty = args.get("pty").and_then(Value::as_bool).unwrap_or(true);
            let terminal_size = parse_size(args.get("size"))?;
            let output_bytes = args
                .get("outputBytes")
                .and_then(Value::as_u64)
                .map(usize::try_from)
                .transpose()
                .map_err(|_| anyhow!("outputBytes is too large"))?;
            let timeout_ms = args.get("timeoutMs").and_then(Value::as_u64);
            let label = args.get("label").and_then(Value::as_str).map(str::to_owned);
            serde_json::to_value(
                manager
                    .spawn(
                        owner_id.clone(),
                        ProcessSpawnSpec {
                            argv,
                            cwd: process_cwd,
                            env,
                            tty,
                            terminal_size,
                            label,
                            timeout_ms,
                            output_bytes,
                        },
                    )
                    .await?,
            )?
        }
        "ps" => serde_json::to_value(manager.list(owner_id))?,
        "describe" => serde_json::to_value(manager.describe(owner_id, &process_id(&args)?)?)?,
        "logs" => {
            let cursor = args.get("cursor").and_then(Value::as_u64).unwrap_or(0);
            let max_bytes = args
                .get("maxBytes")
                .and_then(Value::as_u64)
                .map(usize::try_from)
                .transpose()
                .map_err(|_| anyhow!("maxBytes is too large"))?;
            let follow = args.get("follow").and_then(Value::as_bool).unwrap_or(false);
            let timeout = args
                .get("timeoutMs")
                .and_then(Value::as_u64)
                .map(Duration::from_millis);
            let id = process_id(&args)?;
            let logs = manager.logs(owner_id, &id, cursor, max_bytes, follow, timeout);
            serde_json::to_value(tokio::select! {
                result = logs => result?,
                () = abort.cancelled() => bail!("Operation aborted"),
            })?
        }
        "send" => {
            let id = process_id(&args)?;
            if let Some(text) = args.get("text").and_then(Value::as_str) {
                manager.write(owner_id, &id, text.as_bytes().to_vec(), false).await?;
            }
            if let Some(encoded) = args.get("dataBase64").and_then(Value::as_str) {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(encoded)
                    .map_err(|_| anyhow!("dataBase64 is invalid"))?;
                manager.write(owner_id, &id, bytes, false).await?;
            }
            if let Some(keys) = args.get("keys").and_then(Value::as_array) {
                let keys = keys
                    .iter()
                    .map(parse_key)
                    .collect::<Result<Vec<_>>>()?;
                manager.send_keys(owner_id, &id, &keys).await?;
            }
            if let Some(signal) = args.get("signal") {
                manager.signal(owner_id, &id, parse_signal(signal)?)?;
            }
            if args
                .get("closeStdin")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                manager.write(owner_id, &id, Vec::new(), true).await?;
            }
            json!({ "ok": true })
        }
        "resize" => {
            let size = parse_size(args.get("size"))?
                .ok_or_else(|| anyhow!("resize requires size"))?;
            manager.resize(owner_id, &process_id(&args)?, size)?;
            json!({ "ok": true })
        }
        "signal" => {
            let signal = parse_signal(
                args.get("signal")
                    .ok_or_else(|| anyhow!("signal requires signal"))?,
            )?;
            manager.signal(owner_id, &process_id(&args)?, signal)?;
            json!({ "ok": true })
        }
        "stop" => {
            let timeout = args
                .get("timeoutMs")
                .and_then(Value::as_u64)
                .map(Duration::from_millis);
            let id = process_id(&args)?;
            let stop = manager.stop(owner_id, &id, timeout);
            serde_json::to_value(tokio::select! {
                result = stop => result?,
                () = abort.cancelled() => bail!("Operation aborted"),
            })?
        }
        "wait" => {
            let timeout = args
                .get("timeoutMs")
                .and_then(Value::as_u64)
                .map(Duration::from_millis);
            let id = process_id(&args)?;
            let wait = manager.wait(owner_id, &id, timeout);
            serde_json::to_value(tokio::select! {
                result = wait => result?,
                () = abort.cancelled() => bail!("Operation aborted"),
            })?
        }
        _ => bail!("unknown process operation: {op}"),
    };

    Ok(AgentToolResult {
        content: vec![pi_ai::ContentBlock::text(serde_json::to_string_pretty(&details)?)],
        details,
        ..Default::default()
    })
}

fn process_id(args: &Value) -> Result<ProcessId> {
    serde_json::from_value(
        args.get("id")
            .cloned()
            .ok_or_else(|| anyhow!("process operation requires id"))?,
    )
    .map_err(Into::into)
}

fn parse_env(value: Option<&Value>) -> Result<BTreeMap<String, Option<String>>> {
    let Some(object) = value.and_then(Value::as_object) else {
        return Ok(BTreeMap::new());
    };
    object
        .iter()
        .map(|(key, value)| {
            if value.is_null() {
                Ok((key.clone(), None))
            } else {
                value
                    .as_str()
                    .map(|value| (key.clone(), Some(value.to_owned())))
                    .ok_or_else(|| anyhow!("environment values must be strings or null"))
            }
        })
        .collect()
}

fn parse_size(value: Option<&Value>) -> Result<Option<ProcessTerminalSize>> {
    let Some(value) = value else {
        return Ok(None);
    };
    Ok(Some(serde_json::from_value(value.clone())?))
}

fn parse_key(value: &Value) -> Result<ProcessKey> {
    serde_json::from_value(value.clone()).map_err(Into::into)
}

fn parse_signal(value: &Value) -> Result<ProcessSignal> {
    serde_json::from_value(value.clone()).map_err(Into::into)
}

fn process_schema() -> Schema {
    let string = |description: &str| Schema {
        schema_type: Some(Value::String("string".to_owned())),
        description: Some(description.to_owned()),
        ..Schema::default()
    };
    let integer = |description: &str| Schema {
        schema_type: Some(Value::String("integer".to_owned())),
        description: Some(description.to_owned()),
        minimum: Some(0.0),
        ..Schema::default()
    };
    let boolean = |description: &str| Schema {
        schema_type: Some(Value::String("boolean".to_owned())),
        description: Some(description.to_owned()),
        ..Schema::default()
    };
    let array = |items: Schema, description: &str| Schema {
        schema_type: Some(Value::String("array".to_owned())),
        items: Some(Box::new(items)),
        description: Some(description.to_owned()),
        ..Schema::default()
    };
    let object = |description: &str| Schema {
        schema_type: Some(Value::String("object".to_owned())),
        description: Some(description.to_owned()),
        additional_properties: Some(Value::Bool(true)),
        ..Schema::default()
    };

    let mut op = string("Operation: start, ps, describe, logs, send, resize, signal, stop, or wait");
    op.enum_values = ["start", "ps", "describe", "logs", "send", "resize", "signal", "stop", "wait"]
        .into_iter()
        .map(|value| Value::String(value.to_owned()))
        .collect();
    let fields = vec![
        ("op".to_owned(), op, true),
        ("id".to_owned(), string("Opaque process id returned by start"), false),
        ("argv".to_owned(), array(string("One argv entry"), "Application and arguments; no shell interpolation"), false),
        ("cwd".to_owned(), string("Absolute working directory"), false),
        ("env".to_owned(), object("Environment overrides; null unsets a variable"), false),
        ("pty".to_owned(), boolean("Attach a PTY; defaults true"), false),
        ("size".to_owned(), object("PTY size with rows and cols"), false),
        ("label".to_owned(), string("Human-readable process label"), false),
        ("timeoutMs".to_owned(), integer("Operation or process timeout in milliseconds"), false),
        ("outputBytes".to_owned(), integer("Bounded retained output bytes"), false),
        ("cursor".to_owned(), integer("Monotonic byte cursor"), false),
        ("maxBytes".to_owned(), integer("Maximum bytes returned by logs"), false),
        ("follow".to_owned(), boolean("Wait for output after cursor"), false),
        ("text".to_owned(), string("UTF-8 stdin text"), false),
        ("dataBase64".to_owned(), string("Raw stdin bytes as base64"), false),
        ("keys".to_owned(), array(string("ENTER, TAB, ESCAPE, CTRL_C, CTRL_D, UP, DOWN, LEFT, RIGHT"), "PTY keys"), false),
        ("signal".to_owned(), string("SIGINT, SIGTERM, SIGHUP, SIGQUIT, or SIGKILL"), false),
        ("closeStdin".to_owned(), boolean("Close stdin after prior writes"), false),
    ];
    let mut schema = Schema::object_ordered(fields);
    schema.additional_properties = Some(Value::Bool(false));
    schema
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_tool_carries_exec_capability() {
        let tool = process_tool(
            Path::new("."),
            ProcessManager::with_config(super::super::ProcessManagerConfig {
                idle_timeout: None,
                ..super::super::ProcessManagerConfig::default()
            }),
            ProcessOwnerId::new("test-owner"),
        );
        assert_eq!(tool.capability, ToolCapability::Exec);
    }
}
