use anyhow::{Result, anyhow};
use pi_coding::{Application, LoopCreateRequest, LoopUpdateRequest};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InteractiveLoopCommand {
    Create { interval: String, prompt: String },
    List,
    Update {
        task_id: String,
        interval: Option<String>,
        prompt: Option<String>,
    },
    Delete { task_id: String },
    Cancel { task_id: String },
}

pub fn parse_interactive_loop_command(
    name: &str,
    argument: Option<&str>,
) -> Result<Option<InteractiveLoopCommand>> {
    let argument = argument.unwrap_or_default().trim();
    Ok(Some(match name {
        "loop" => parse_loop_invocation(argument)?,
        "loops" => InteractiveLoopCommand::List,
        "loop-update" => parse_update(argument)?,
        "loop-delete" => InteractiveLoopCommand::Delete {
            task_id: required_id(argument, "/loop delete <id>")?,
        },
        "loop-cancel" => InteractiveLoopCommand::Cancel {
            task_id: required_id(argument, "/loop cancel <id>")?,
        },
        _ => return Ok(None),
    }))
}

/// Parse the `/loop` primary surface: subcommand style
/// `list | cancel <id> | delete <id> | update <id> [interval] [prompt] | create <interval> <prompt>`,
/// falling back to the legacy bare create form `/loop <interval> <prompt>` when the
/// first token is not a subcommand keyword. Subcommand keywords are never valid
/// interval tokens, so the two forms cannot be ambiguous.
fn parse_loop_invocation(argument: &str) -> Result<InteractiveLoopCommand> {
    let (subcommand, rest) = argument
        .split_once(char::is_whitespace)
        .map_or((argument, ""), |(head, tail)| (head, tail.trim_start()));
    match subcommand {
        "list" => {
            if !rest.is_empty() {
                return Err(anyhow!("usage: /loop list"));
            }
            Ok(InteractiveLoopCommand::List)
        }
        "cancel" => Ok(InteractiveLoopCommand::Cancel {
            task_id: required_id(rest, "/loop cancel <id>")?,
        }),
        "delete" => Ok(InteractiveLoopCommand::Delete {
            task_id: required_id(rest, "/loop delete <id>")?,
        }),
        "update" => parse_update(rest),
        "create" => parse_create(rest),
        _ => parse_create(argument),
    }
}

fn parse_create(argument: &str) -> Result<InteractiveLoopCommand> {
    let parsed = pi_coding::parse_loop_args(argument);
    let interval = parsed
        .interval
        .ok_or_else(|| anyhow!("{}", pi_coding::loop_usage_message()))?;
    Ok(InteractiveLoopCommand::Create {
        interval: interval.to_owned(),
        prompt: parsed.prompt.to_owned(),
    })
}

pub async fn execute_interactive_loop_command(
    application: &Application,
    command: InteractiveLoopCommand,
) -> Result<String> {
    match command {
        InteractiveLoopCommand::Create { interval, prompt } => {
            let task = application
                .loop_create(LoopCreateRequest::immediate(interval, prompt))
                .await?;
            Ok(format!(
                "scheduled {} · {} · expires {}",
                task.id,
                task.human_schedule(),
                task.expires_at.to_rfc3339()
            ))
        }
        InteractiveLoopCommand::List => {
            let tasks = application.loop_list().await?;
            if tasks.is_empty() {
                return Ok("no active loops".to_owned());
            }
            Ok(tasks
                .iter()
                .map(|task| {
                    format!(
                        "{}  {}  next {}  {}",
                        task.id,
                        task.human_schedule(),
                        task.next_fire_at().to_rfc3339(),
                        task.prompt
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"))
        }
        InteractiveLoopCommand::Update {
            task_id,
            interval,
            prompt,
        } => {
            let task = application
                .loop_update(LoopUpdateRequest {
                    task_id,
                    interval,
                    prompt,
                })
                .await?;
            Ok(format!(
                "updated loop {} · {} · next {} · {}",
                task.id,
                task.human_schedule(),
                task.next_fire_at().to_rfc3339(),
                task.prompt
            ))
        }
        InteractiveLoopCommand::Delete { task_id } => {
            if application.loop_delete(&task_id).await? {
                Ok(format!("deleted loop {task_id}"))
            } else {
                Err(anyhow!("no active loop with id {task_id}"))
            }
        }
        InteractiveLoopCommand::Cancel { task_id } => {
            if application.loop_cancel(&task_id).await? {
                Ok(format!("cancelled loop {task_id}"))
            } else {
                Err(anyhow!("no active loop with id {task_id}"))
            }
        }
    }
}

fn parse_update(argument: &str) -> Result<InteractiveLoopCommand> {
    let mut parts = argument.split_whitespace();
    let task_id = parts
        .next()
        .ok_or_else(|| anyhow!("usage: /loop update <id> [interval] [prompt]"))?
        .to_owned();
    let fields = parts.collect::<Vec<_>>();
    if fields.is_empty() {
        return Err(anyhow!("usage: /loop update <id> [interval] [prompt]"));
    }
    let (interval, prompt) = if pi_coding::is_interval_token(fields[0]) {
        (
            Some(fields[0].to_owned()),
            (fields.len() > 1).then(|| fields[1..].join(" ")),
        )
    } else {
        (None, Some(fields.join(" ")))
    };
    Ok(InteractiveLoopCommand::Update {
        task_id,
        interval,
        prompt,
    })
}

fn required_id(argument: &str, usage: &str) -> Result<String> {
    let mut parts = argument.split_whitespace();
    let task_id = parts.next().ok_or_else(|| anyhow!("usage: {usage}"))?;
    if parts.next().is_some() {
        return Err(anyhow!("usage: {usage}"));
    }
    Ok(task_id.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_advertised_loop_operation() {
        assert!(matches!(
            parse_interactive_loop_command("loop", Some("5m check deploy")).unwrap(),
            Some(InteractiveLoopCommand::Create { interval, prompt })
                if interval == "5m" && prompt == "check deploy"
        ));
        assert!(matches!(
            parse_interactive_loop_command("loop", Some("3s echo hello")).unwrap(),
            Some(InteractiveLoopCommand::Create { interval, prompt })
                if interval == "3s" && prompt == "echo hello"
        ));
        assert!(matches!(
            parse_interactive_loop_command("loop", Some("300 echo hello")).unwrap(),
            Some(InteractiveLoopCommand::Create { interval, prompt })
                if interval == "300" && prompt == "echo hello"
        ));
        assert!(matches!(
            parse_interactive_loop_command("loops", None).unwrap(),
            Some(InteractiveLoopCommand::List)
        ));
        // Primary subcommand style.
        assert!(matches!(
            parse_interactive_loop_command("loop", Some("list")).unwrap(),
            Some(InteractiveLoopCommand::List)
        ));
        assert!(matches!(
            parse_interactive_loop_command("loop", Some("create 5m check deploy")).unwrap(),
            Some(InteractiveLoopCommand::Create { interval, prompt })
                if interval == "5m" && prompt == "check deploy"
        ));
        assert!(matches!(
            parse_interactive_loop_command("loop", Some("cancel abc123")).unwrap(),
            Some(InteractiveLoopCommand::Cancel { task_id }) if task_id == "abc123"
        ));
        assert!(matches!(
            parse_interactive_loop_command("loop", Some("delete abc123")).unwrap(),
            Some(InteractiveLoopCommand::Delete { task_id }) if task_id == "abc123"
        ));
        assert!(matches!(
            parse_interactive_loop_command("loop", Some("update abc123 10m check again")).unwrap(),
            Some(InteractiveLoopCommand::Update { task_id, interval: Some(interval), prompt: Some(prompt) })
                if task_id == "abc123" && interval == "10m" && prompt == "check again"
        ));
        assert!(matches!(
            parse_interactive_loop_command("loop", Some("update abc123 prompt only")).unwrap(),
            Some(InteractiveLoopCommand::Update { task_id, interval: None, prompt: Some(prompt) })
                if task_id == "abc123" && prompt == "prompt only"
        ));
        // A leading interval wins over subcommand keywords: no ambiguity.
        assert!(matches!(
            parse_interactive_loop_command("loop", Some("5m list todos")).unwrap(),
            Some(InteractiveLoopCommand::Create { interval, prompt })
                if interval == "5m" && prompt == "list todos"
        ));
        // Legacy aliases keep working.
        assert!(matches!(
            parse_interactive_loop_command("loop-update", Some("abc123 10m check again")).unwrap(),
            Some(InteractiveLoopCommand::Update { task_id, interval: Some(interval), prompt: Some(prompt) })
                if task_id == "abc123" && interval == "10m" && prompt == "check again"
        ));
        assert!(matches!(
            parse_interactive_loop_command("loop-update", Some("abc123 prompt only")).unwrap(),
            Some(InteractiveLoopCommand::Update { task_id, interval: None, prompt: Some(prompt) })
                if task_id == "abc123" && prompt == "prompt only"
        ));
        assert!(matches!(
            parse_interactive_loop_command("loop-delete", Some("abc123")).unwrap(),
            Some(InteractiveLoopCommand::Delete { task_id }) if task_id == "abc123"
        ));
        assert!(matches!(
            parse_interactive_loop_command("loop-cancel", Some("abc123")).unwrap(),
            Some(InteractiveLoopCommand::Cancel { task_id }) if task_id == "abc123"
        ));
        // Subcommand usage errors surface the canonical /loop syntax.
        let err = parse_interactive_loop_command("loop", Some("update abc123")).unwrap_err();
        assert!(
            err.to_string().contains("usage: /loop update <id> [interval] [prompt]"),
            "loop update usage: {err:#}"
        );
        assert!(parse_interactive_loop_command("loop", Some("list extra")).is_err());
        assert!(parse_interactive_loop_command("loop", Some("cancel")).is_err());
        assert!(parse_interactive_loop_command("loop", Some("cancel abc123 extra")).is_err());
        assert!(parse_interactive_loop_command("loop", Some("delete abc123 extra")).is_err());
        assert!(parse_interactive_loop_command("loop", Some("create")).is_err());
        assert!(parse_interactive_loop_command("loop", Some("create 5m")).is_err());
        assert!(parse_interactive_loop_command("loop-update", Some("abc123")).is_err());
        assert!(parse_interactive_loop_command("loop-delete", Some("abc123 extra")).is_err());
        assert!(parse_interactive_loop_command("loop-cancel", Some("abc123 extra")).is_err());
    }
}
