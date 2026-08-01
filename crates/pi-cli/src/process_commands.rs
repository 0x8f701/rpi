use std::path::Path;
use std::time::Duration;

use anyhow::{Result, anyhow};
use base64::Engine as _;
use pi_coding::{
    Application, ProcessId, ProcessInfo, ProcessKey, ProcessLogs, ProcessSignal, ProcessSpawnSpec,
    ProcessTerminalSize,
};


#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InteractiveProcessCommand {
    Ps,
    Start { argv: Vec<String>, tty: bool },
    Describe { id: ProcessId },
    Logs { id: ProcessId, cursor: u64, follow: bool },
    Send { id: ProcessId, text: String },
    Resize { id: ProcessId, size: ProcessTerminalSize },
    Signal { id: ProcessId, signal: ProcessSignal },
    Stop { id: ProcessId },
    Wait { id: ProcessId },
}

pub fn parse_interactive_process_command(name: &str, argument: Option<&str>) -> Result<Option<InteractiveProcessCommand>> {
    if name == "ps" {
        return Ok(Some(InteractiveProcessCommand::Ps));
    }
    if name != "process" {
        return Ok(None);
    }
    let argument = argument.unwrap_or_default();
    let arguments = pi_coding::parse_command_args(argument);
    let mut parts = arguments.iter().map(String::as_str);
    let operation = parts.next().unwrap_or_default();
    let parse_id = |value: &str| -> Result<ProcessId> {
        Ok(serde_json::from_value(serde_json::Value::String(value.to_owned()))?)
    };
    Ok(Some(match operation {
        "start" => {
            let mut argv = parts.map(str::to_owned).collect::<Vec<_>>();
            let tty = argv.first().is_some_and(|value| value == "--tty");
            if tty { argv.remove(0); }
            if argv.is_empty() { return Err(anyhow!("process start requires [--tty] <program> [args...]")); }
            InteractiveProcessCommand::Start { argv, tty }
        }
        "describe" => InteractiveProcessCommand::Describe { id: parse_id(parts.next().ok_or_else(|| anyhow!("process describe requires an opaque process id"))?)? },
        "logs" => {
            let id = parse_id(parts.next().ok_or_else(|| anyhow!("process logs requires an opaque process id"))?)?;
            let mut cursor = 0;
            let mut follow = false;
            while let Some(option) = parts.next() {
                match option {
                    "--follow" | "-f" => follow = true,
                    "--cursor" => cursor = parts.next().ok_or_else(|| anyhow!("process logs --cursor requires a byte offset"))?.parse()?,
                    value => return Err(anyhow!("unknown process logs option {value:?}")),
                }
            }
            InteractiveProcessCommand::Logs { id, cursor, follow }
        }
        "send" => {
            let id = parse_id(parts.next().ok_or_else(|| anyhow!("process send requires an opaque process id"))?)?;
            let text = parts.collect::<Vec<_>>().join(" ");
            if text.is_empty() { return Err(anyhow!("process send requires text")); }
            InteractiveProcessCommand::Send { id, text }
        }
        "resize" => {
            let id = parse_id(parts.next().ok_or_else(|| anyhow!("process resize requires an opaque process id"))?)?;
            let rows = parts.next().ok_or_else(|| anyhow!("process resize requires <rows> <cols>"))?.parse()?;
            let cols = parts.next().ok_or_else(|| anyhow!("process resize requires <rows> <cols>"))?.parse()?;
            InteractiveProcessCommand::Resize { id, size: ProcessTerminalSize { rows, cols } }
        }
        "signal" => {
            let id = parse_id(parts.next().ok_or_else(|| anyhow!("process signal requires an opaque process id"))?)?;
            let signal = parts.next().ok_or_else(|| anyhow!("process signal requires SIGINT|SIGTERM|SIGHUP|SIGQUIT|SIGKILL"))?;
            InteractiveProcessCommand::Signal { id, signal: serde_json::from_value(serde_json::Value::String(signal.to_owned()))? }
        }
        "stop" => InteractiveProcessCommand::Stop { id: parse_id(parts.next().ok_or_else(|| anyhow!("process stop requires an opaque process id"))?)? },
        "wait" => InteractiveProcessCommand::Wait { id: parse_id(parts.next().ok_or_else(|| anyhow!("process wait requires an opaque process id"))?)? },
        _ => return Err(anyhow!("usage: /process <start|describe|logs|send|resize|signal|stop|wait> ...")),
    }))
}

pub async fn execute_interactive_process_command(
    application: &Application,
    command: InteractiveProcessCommand,
) -> Result<String> {
    match command {
        InteractiveProcessCommand::Ps => Ok(format_process_list(&application.process_list())),
        InteractiveProcessCommand::Start { argv, tty } => Ok(format_process_info(&start_process(application, argv, application.session().cwd(), tty, None).await?)),
        InteractiveProcessCommand::Describe { id } => Ok(format_process_info(&application.process_describe(&id)?)),
        InteractiveProcessCommand::Logs { id, cursor, follow } => Ok(format_process_logs(&application.process_logs(&id, cursor, None, follow, Some(Duration::from_secs(30))).await?)),
        InteractiveProcessCommand::Send { id, text } => { application.process_write(&id, text.into_bytes(), false).await?; Ok("input sent".to_owned()) }
        InteractiveProcessCommand::Resize { id, size } => { application.process_resize(&id, size)?; Ok("terminal resized".to_owned()) }
        InteractiveProcessCommand::Signal { id, signal } => { application.process_signal(&id, signal)?; Ok("signal sent".to_owned()) }
        InteractiveProcessCommand::Stop { id } => Ok(format_process_info(&application.process_stop(&id, None).await?)),
        InteractiveProcessCommand::Wait { id } => Ok(format_process_info(&application.process_wait(&id, None).await?)),
    }
}

pub async fn start_process(
    application: &Application,
    argv: Vec<String>,
    cwd: &Path,
    tty: bool,
    size: Option<ProcessTerminalSize>,
) -> Result<ProcessInfo> {
    application
        .process_spawn(ProcessSpawnSpec {
            argv,
            cwd: cwd.to_path_buf(),
            env: Default::default(),
            tty,
            terminal_size: size,
            label: None,
            timeout_ms: None,
            output_bytes: None,
        })
        .await
}

pub async fn send_process_keys(
    application: &Application,
    id: &ProcessId,
    keys: &[ProcessKey],
) -> Result<()> {
    application.process_send_keys(id, keys).await
}

#[must_use]
pub fn format_process_list(processes: &[ProcessInfo]) -> String {
    if processes.is_empty() {
        return "No supervised processes".to_owned();
    }
    processes
        .iter()
        .map(format_process_info)
        .collect::<Vec<_>>()
        .join("\n")
}

#[must_use]
pub fn format_process_info(process: &ProcessInfo) -> String {
    format!(
        "{}\t{:?}\t{}\tcursor {}..{}",
        process.id,
        process.state,
        process.label.as_deref().unwrap_or("(unlabeled)"),
        process.output_start_cursor,
        process.output_cursor
    )
}

#[must_use]
pub fn format_process_logs(logs: &ProcessLogs) -> String {
    let mut text = logs
        .chunks
        .iter()
        .filter_map(|chunk| base64::engine::general_purpose::STANDARD.decode(&chunk.data_base64).ok())
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .collect::<String>();
    if logs.lost {
        text.insert_str(0, &format!("[{} output bytes lost before cursor {}]\n", logs.lost_bytes, logs.start_cursor));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_advertised_process_operation() {
        assert!(matches!(parse_interactive_process_command("ps", None).unwrap(), Some(InteractiveProcessCommand::Ps)));
        assert!(matches!(parse_interactive_process_command("process", Some("start --tty echo ok")).unwrap(), Some(InteractiveProcessCommand::Start { tty: true, .. })));
        assert!(matches!(parse_interactive_process_command("process", Some("describe id")).unwrap(), Some(InteractiveProcessCommand::Describe { .. })));
        assert!(matches!(parse_interactive_process_command("process", Some("logs id --cursor 42 --follow")).unwrap(), Some(InteractiveProcessCommand::Logs { cursor: 42, follow: true, .. })));
        assert!(matches!(parse_interactive_process_command("process", Some("send id \"hello world\"")).unwrap(), Some(InteractiveProcessCommand::Send { text, .. }) if text == "hello world"));
        assert!(matches!(parse_interactive_process_command("process", Some("resize id 40 120")).unwrap(), Some(InteractiveProcessCommand::Resize { size: ProcessTerminalSize { rows: 40, cols: 120 }, .. })));
        assert!(matches!(parse_interactive_process_command("process", Some("signal id SIGINT")).unwrap(), Some(InteractiveProcessCommand::Signal { .. })));
        assert!(matches!(parse_interactive_process_command("process", Some("stop id")).unwrap(), Some(InteractiveProcessCommand::Stop { .. })));
        assert!(matches!(parse_interactive_process_command("process", Some("wait id")).unwrap(), Some(InteractiveProcessCommand::Wait { .. })));
        assert!(parse_interactive_process_command("process", Some("send id")).is_err());
    }
}
