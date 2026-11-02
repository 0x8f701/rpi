use pi_coding::{Application, TodoChildAddTaskRequest, TodoChildCreateRequest, TodoOp};

#[derive(Debug)]
pub(crate) enum TodoChildCommand {
    New {
        label: Option<String>,
    },
    List,
    Select {
        child_session_id: String,
    },
    Send {
        child_session_id: Option<String>,
        message: String,
    },
    Add {
        child_session_id: String,
        content: String,
        depends_on: Vec<String>,
        agent: Option<String>,
        role: Option<String>,
        expected_revision: Option<u64>,
    },
    Status {
        child_session_id: Option<String>,
    },
    Stop {
        child_session_id: Option<String>,
    },
    Pause {
        child_session_id: Option<String>,
    },
    Resume {
        child_session_id: Option<String>,
    },
    Cancel {
        child_session_id: Option<String>,
    },
    Archive {
        child_session_id: String,
    },
    Remove {
        child_session_id: String,
    },
}

#[derive(Debug)]
pub(crate) enum TodoCommand {
    Open,
    Child(TodoChildCommand),
    Dag(TodoOp),
    Markdown(String),
}

const TODO_LIST_USAGE: &str = "usage: /todo list";
const TODO_SELECT_USAGE: &str = "usage: /todo select <id>";
const TODO_SEND_USAGE: &str = "usage: /todo send [id] <message>";
const TODO_ADD_USAGE: &str =
    "usage: /todo add <id> [--revision N] [--depends id,id] [--agent name] [--role role] <task>";
const TODO_STATUS_USAGE: &str = "usage: /todo status [id]";
const TODO_STOP_USAGE: &str = "usage: /todo stop [id]";
const TODO_PAUSE_USAGE: &str = "usage: /todo pause [id]";
const TODO_RESUME_USAGE: &str = "usage: /todo resume [id]";
const TODO_CANCEL_USAGE: &str = "usage: /todo cancel [id]";
const TODO_ARCHIVE_USAGE: &str = "usage: /todo archive <id>";
const TODO_REMOVE_USAGE: &str = "usage: /todo remove <id>";
const TODO_START_USAGE: &str = "usage: /todo start <task>";
const TODO_DONE_USAGE: &str = "usage: /todo done <task>";
const TODO_DROP_USAGE: &str = "usage: /todo drop <task>";
const TODO_CLEAR_USAGE: &str = "usage: /todo clear";
pub(crate) const TODO_COMMAND_USAGE: &str = "Usage: /todo [list|new [label]|select <id>|send [id] <message>|\
     add <id> [options] <task>|status [id]|pause [id]|resume [id]|stop [id]|cancel [id]|\
     archive <id>|remove <id>|start <task>|done <task>|drop <task>|clear|<markdown>] \
     (child verbs drive autonomous child sessions; start/done/drop match exact task text, \
     spaces allowed; <markdown> sets the plan)";
const TODO_CHILD_VERBS: [&str; 12] = [
    "new",
    "list",
    "select",
    "send",
    "add",
    "status",
    "pause",
    "resume",
    "stop",
    "cancel",
    "archive",
    "remove",
];

/// True when the first `/todo` token names durable-child semantics.
///
/// Unified `/todo` dispatch checks these verbs before DAG verbs and Markdown.
pub(crate) fn is_todo_child_verb(arg: &str) -> bool {
    arg.split_whitespace().next().is_some_and(|verb| {
        TODO_CHILD_VERBS
            .iter()
            .any(|candidate| verb.eq_ignore_ascii_case(candidate))
    })
}

pub(crate) fn looks_like_todo_markdown(argument: &str) -> bool {
    argument
        .lines()
        .find(|line| !line.trim().is_empty())
        .is_some_and(|line| {
            let line = line.trim_start();
            line.starts_with('#')
                || line.starts_with("- ")
                || line.starts_with("* ")
                || line.starts_with("+ ")
        })
}

pub(crate) fn parse_todo_command(argument: &str) -> Result<TodoCommand, String> {
    let argument = argument.trim();
    if argument.is_empty() || argument.eq_ignore_ascii_case("list") {
        return Ok(TodoCommand::Open);
    }
    if is_todo_child_verb(argument) {
        return parse_todo_child_command(argument).map(TodoCommand::Child);
    }
    let (verb, rest) = argument
        .split_once(char::is_whitespace)
        .map_or((argument, ""), |(verb, rest)| (verb, rest.trim()));
    if verb.eq_ignore_ascii_case("clear") {
        if !rest.is_empty() {
            return Err(TODO_CLEAR_USAGE.to_owned());
        }
        return Ok(TodoCommand::Dag(TodoOp::Rm {
            task: None,
            phase: None,
            cascade: true,
        }));
    }
    if verb.eq_ignore_ascii_case("start") {
        if rest.is_empty() {
            return Err(TODO_START_USAGE.to_owned());
        }
        return Ok(TodoCommand::Dag(TodoOp::Start {
            task: rest.to_owned(),
        }));
    }
    if verb.eq_ignore_ascii_case("done") {
        if rest.is_empty() {
            return Err(TODO_DONE_USAGE.to_owned());
        }
        return Ok(TodoCommand::Dag(TodoOp::Done {
            task: Some(rest.to_owned()),
            phase: None,
        }));
    }
    if verb.eq_ignore_ascii_case("drop") {
        if rest.is_empty() {
            return Err(TODO_DROP_USAGE.to_owned());
        }
        return Ok(TodoCommand::Dag(TodoOp::Drop {
            task: Some(rest.to_owned()),
            phase: None,
        }));
    }
    if looks_like_todo_markdown(argument) {
        return Ok(TodoCommand::Markdown(argument.to_owned()));
    }
    Err(TODO_COMMAND_USAGE.to_owned())
}

pub(crate) fn parse_todo_child_command(arg: &str) -> Result<TodoChildCommand, String> {
    let argument = arg.trim();
    let (subcommand, rest) = argument
        .split_once(char::is_whitespace)
        .map_or((argument, ""), |(subcommand, rest)| {
            (subcommand, rest.trim())
        });
    if subcommand.eq_ignore_ascii_case("new") {
        return Ok(TodoChildCommand::New {
            label: (!rest.is_empty()).then(|| rest.to_owned()),
        });
    }
    if subcommand.eq_ignore_ascii_case("list") {
        return if rest.is_empty() {
            Ok(TodoChildCommand::List)
        } else {
            Err(TODO_LIST_USAGE.to_owned())
        };
    }
    if subcommand.eq_ignore_ascii_case("select") {
        return Ok(TodoChildCommand::Select {
            child_session_id: parse_child_id(rest, TODO_SELECT_USAGE)?,
        });
    }
    if subcommand.eq_ignore_ascii_case("send") {
        if rest.is_empty() {
            return Err(TODO_SEND_USAGE.to_owned());
        }
        return match rest.split_once(char::is_whitespace) {
            Some((first, message)) if uuid_like(first) => Ok(TodoChildCommand::Send {
                child_session_id: Some(first.to_owned()),
                message: message.trim().to_owned(),
            }),
            _ => Ok(TodoChildCommand::Send {
                child_session_id: None,
                message: rest.to_owned(),
            }),
        };
    }
    if subcommand.eq_ignore_ascii_case("add") {
        return parse_todo_add(rest);
    }
    if subcommand.eq_ignore_ascii_case("status") {
        return Ok(TodoChildCommand::Status {
            child_session_id: parse_optional_child_id(rest, TODO_STATUS_USAGE)?,
        });
    }
    if subcommand.eq_ignore_ascii_case("stop") {
        return Ok(TodoChildCommand::Stop {
            child_session_id: parse_optional_child_id(rest, TODO_STOP_USAGE)?,
        });
    }
    if subcommand.eq_ignore_ascii_case("pause") {
        return Ok(TodoChildCommand::Pause {
            child_session_id: parse_optional_child_id(rest, TODO_PAUSE_USAGE)?,
        });
    }
    if subcommand.eq_ignore_ascii_case("resume") {
        return Ok(TodoChildCommand::Resume {
            child_session_id: parse_optional_child_id(rest, TODO_RESUME_USAGE)?,
        });
    }
    if subcommand.eq_ignore_ascii_case("cancel") {
        return Ok(TodoChildCommand::Cancel {
            child_session_id: parse_optional_child_id(rest, TODO_CANCEL_USAGE)?,
        });
    }
    if subcommand.eq_ignore_ascii_case("archive") {
        return Ok(TodoChildCommand::Archive {
            child_session_id: parse_child_id(rest, TODO_ARCHIVE_USAGE)?,
        });
    }
    if subcommand.eq_ignore_ascii_case("remove") {
        return Ok(TodoChildCommand::Remove {
            child_session_id: parse_child_id(rest, TODO_REMOVE_USAGE)?,
        });
    }
    Err(format!("unknown /todo subcommand: {subcommand}"))
}

fn parse_child_id(rest: &str, usage: &str) -> Result<String, String> {
    parse_optional_child_id(rest, usage)?.ok_or_else(|| usage.to_owned())
}

fn parse_optional_child_id(rest: &str, usage: &str) -> Result<Option<String>, String> {
    let mut parts = rest.split_whitespace();
    let child_session_id = parts.next().map(str::to_owned);
    if parts.next().is_some() {
        return Err(usage.to_owned());
    }
    Ok(child_session_id)
}

fn parse_todo_add(rest: &str) -> Result<TodoChildCommand, String> {
    const USAGE: &str = TODO_ADD_USAGE;
    let tokens = rest.split_whitespace().collect::<Vec<_>>();
    let child_session_id = tokens.first().ok_or(USAGE)?.to_string();
    let mut expected_revision = None;
    let mut depends_on = Vec::new();
    let mut agent = None;
    let mut role = None;
    let mut content_parts = Vec::new();
    let mut index = 1;
    while index < tokens.len() {
        match tokens[index] {
            "--revision" | "--rev" => {
                index += 1;
                let value = tokens.get(index).ok_or(USAGE)?;
                expected_revision = Some(
                    value
                        .parse()
                        .map_err(|_| "revision must be an integer".to_owned())?,
                );
            }
            "--depends" => {
                index += 1;
                let value = tokens.get(index).ok_or(USAGE)?;
                depends_on = value
                    .split(',')
                    .filter(|item| !item.is_empty())
                    .map(str::to_owned)
                    .collect();
            }
            "--agent" => {
                index += 1;
                agent = Some(tokens.get(index).ok_or(USAGE)?.to_string());
            }
            "--role" => {
                index += 1;
                role = Some(tokens.get(index).ok_or(USAGE)?.to_string());
            }
            other => content_parts.push(other.to_owned()),
        }
        index += 1;
    }
    let content = content_parts.join(" ");
    if content.is_empty() {
        return Err(USAGE.to_owned());
    }
    Ok(TodoChildCommand::Add {
        child_session_id,
        content,
        depends_on,
        agent,
        role,
        expected_revision,
    })
}

fn uuid_like(value: &str) -> bool {
    value.len() == 36 && value.chars().filter(|character| *character == '-').count() == 4
}

#[derive(Clone, Debug)]
pub(crate) struct TodoChildCommandEffect {
    pub message: String,
    pub is_success: bool,
    pub parent_session_id: Option<String>,
    pub child_session_id: Option<String>,
    pub should_refresh: bool,
    pub children: Option<Vec<pi_coding::TodoChildManifestEntry>>,
    pub refresh_error: Option<String>,
}

impl TodoChildCommandEffect {
    fn success(
        message: String,
        parent_session_id: Option<String>,
        child_session_id: Option<String>,
    ) -> Self {
        Self {
            is_success: true,
            message,
            should_refresh: parent_session_id.is_some(),
            parent_session_id,
            child_session_id,
            children: None,
            refresh_error: None,
        }
    }

    fn failure(message: String, parent_session_id: Option<String>) -> Self {
        Self {
            is_success: false,
            message,
            parent_session_id,
            child_session_id: None,
            should_refresh: false,
            children: None,
            refresh_error: None,
        }
    }
}

pub(crate) async fn execute_todo_child_command(
    application: &Application,
    command: TodoChildCommand,
) -> TodoChildCommandEffect {
    let active_parent = application
        .session()
        .recorder_info()
        .map(|(session_id, _)| session_id);
    let mut effect = match command {
        TodoChildCommand::New { label } => match application
            .todo_child_create(TodoChildCreateRequest {
                label,
                idempotency_key: None,
            })
            .await
        {
            Ok(result) => TodoChildCommandEffect::success(
                format!("created Todo child {}", result.identity.child_session_id),
                Some(result.identity.parent_session_id),
                Some(result.identity.child_session_id),
            ),
            Err(error) => TodoChildCommandEffect::failure(
                format!("failed to create Todo child: {error:#}"),
                active_parent.clone(),
            ),
        },
        TodoChildCommand::List => match application.todo_child_list(active_parent.as_deref()).await
        {
            Ok(entries) => {
                let message = if entries.is_empty() {
                    "No Todo children.".to_owned()
                } else {
                    entries
                        .iter()
                        .map(|entry| {
                            format!(
                                "{}  {}  {:?}  {}",
                                entry.child_session_id,
                                entry.label.as_deref().unwrap_or("(unlabeled)"),
                                entry.status,
                                entry.created_at
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                let mut effect =
                    TodoChildCommandEffect::success(message, active_parent.clone(), None);
                effect.children = Some(entries);
                effect
            }
            Err(error) => TodoChildCommandEffect::failure(
                format!("failed to list Todo children: {error:#}"),
                active_parent.clone(),
            ),
        },
        TodoChildCommand::Select { child_session_id } => match application
            .todo_child_select(active_parent.as_deref(), &child_session_id)
            .await
        {
            Ok(()) => TodoChildCommandEffect::success(
                format!("selected Todo child {child_session_id}"),
                active_parent.clone(),
                Some(child_session_id),
            ),
            Err(error) => TodoChildCommandEffect::failure(
                format!("failed to select: {error:#}"),
                active_parent.clone(),
            ),
        },
        TodoChildCommand::Send {
            child_session_id,
            message,
        } => match application
            .todo_child_send(
                active_parent.as_deref(),
                child_session_id.as_deref(),
                message,
            )
            .await
        {
            Ok(receipt) => TodoChildCommandEffect::success(
                format!(
                    "sent to Todo child {} (receipt {})",
                    receipt.child_session_id, receipt.id
                ),
                Some(receipt.parent_session_id),
                Some(receipt.child_session_id),
            ),
            Err(error) => TodoChildCommandEffect::failure(
                format!("failed to send: {error:#}"),
                active_parent.clone(),
            ),
        },
        TodoChildCommand::Add {
            child_session_id,
            content,
            depends_on,
            agent,
            role,
            expected_revision,
        } => match application
            .todo_child_add_task(
                active_parent.as_deref(),
                &child_session_id,
                TodoChildAddTaskRequest {
                    content,
                    depends_on,
                    agent,
                    role,
                    expected_revision,
                },
            )
            .await
        {
            Ok(result) => TodoChildCommandEffect::success(
                format!(
                    "added task {} (revision {})",
                    result.task.id, result.revision
                ),
                active_parent.clone(),
                Some(child_session_id),
            ),
            Err(error) => TodoChildCommandEffect::failure(
                format!("failed to add task: {error:#}"),
                active_parent.clone(),
            ),
        },
        TodoChildCommand::Status { child_session_id } => match application
            .todo_child_status(active_parent.as_deref(), child_session_id.as_deref())
            .await
        {
            Ok(snapshot) => TodoChildCommandEffect::success(
                format!("{:?}", snapshot),
                Some(snapshot.parent_session_id),
                Some(snapshot.child_session_id),
            ),
            Err(error) => TodoChildCommandEffect::failure(
                format!("failed to get status: {error:#}"),
                active_parent.clone(),
            ),
        },
        TodoChildCommand::Stop { child_session_id } => match application
            .todo_child_stop(active_parent.as_deref(), child_session_id.as_deref())
            .await
        {
            Ok(snapshot) => TodoChildCommandEffect::success(
                format!("stopped: {:?}", snapshot.status),
                Some(snapshot.parent_session_id),
                Some(snapshot.child_session_id),
            ),
            Err(error) => TodoChildCommandEffect::failure(
                format!("failed to stop: {error:#}"),
                active_parent.clone(),
            ),
        },
        TodoChildCommand::Pause { child_session_id } => match application
            .todo_child_pause(active_parent.as_deref(), child_session_id.as_deref())
            .await
        {
            Ok(snapshot) => TodoChildCommandEffect::success(
                format!("paused: {:?}", snapshot.status),
                Some(snapshot.parent_session_id),
                Some(snapshot.child_session_id),
            ),
            Err(error) => TodoChildCommandEffect::failure(
                format!("failed to pause: {error:#}"),
                active_parent.clone(),
            ),
        },
        TodoChildCommand::Resume { child_session_id } => match application
            .todo_child_resume(active_parent.as_deref(), child_session_id.as_deref())
            .await
        {
            Ok(snapshot) => TodoChildCommandEffect::success(
                format!("resumed: {:?}", snapshot.status),
                Some(snapshot.parent_session_id),
                Some(snapshot.child_session_id),
            ),
            Err(error) => TodoChildCommandEffect::failure(
                format!("failed to resume: {error:#}"),
                active_parent.clone(),
            ),
        },
        TodoChildCommand::Cancel { child_session_id } => match application
            .todo_child_cancel(active_parent.as_deref(), child_session_id.as_deref())
            .await
        {
            Ok(Some(snapshot)) => TodoChildCommandEffect::success(
                format!("cancelled: {:?}", snapshot.status),
                Some(snapshot.parent_session_id),
                Some(snapshot.child_session_id),
            ),
            // Idempotent no-op: nothing addressable (e.g. a create already
            // failed and rolled back). Success clears the stale pending UI.
            Ok(None) => TodoChildCommandEffect::success(
                "no Todo child to cancel".to_owned(),
                active_parent.clone(),
                None,
            ),
            Err(error) => TodoChildCommandEffect::failure(
                format!("failed to cancel: {error:#}"),
                active_parent.clone(),
            ),
        },
        TodoChildCommand::Archive { child_session_id } => match application
            .todo_child_archive(active_parent.as_deref(), &child_session_id)
            .await
        {
            Ok(()) => TodoChildCommandEffect::success(
                format!("archived Todo child {child_session_id}"),
                active_parent.clone(),
                Some(child_session_id),
            ),
            Err(error) => TodoChildCommandEffect::failure(
                format!("failed to archive: {error:#}"),
                active_parent.clone(),
            ),
        },
        TodoChildCommand::Remove { child_session_id } => match application
            .todo_child_remove(active_parent.as_deref(), &child_session_id)
            .await
        {
            Ok(()) => TodoChildCommandEffect::success(
                format!("removed Todo child {child_session_id}"),
                active_parent.clone(),
                None,
            ),
            Err(error) => TodoChildCommandEffect::failure(
                format!("failed to remove: {error:#}"),
                active_parent.clone(),
            ),
        },
    };
    if effect.should_refresh && effect.children.is_none() {
        match application.todo_child_list(active_parent.as_deref()).await {
            Ok(children) => effect.children = Some(children),
            Err(error) => {
                let refresh_error = format!("failed to refresh Todo children: {error:#}");
                effect.message = format!("{} · {refresh_error}", effect.message);
                effect.refresh_error = Some(refresh_error);
            }
        }
    }
    effect
}

#[cfg(test)]
mod tests {
    use pi_coding::TodoOp;

    use super::{
        TodoChildCommand, TodoCommand, is_todo_child_verb, parse_todo_child_command,
        parse_todo_command,
    };

    #[test]
    fn unified_todo_child_grammar_is_case_insensitive_and_cardinality_bounded() {
        assert!(is_todo_child_verb("LIST"));
        assert!(matches!(
            parse_todo_child_command("LIST").expect("case-insensitive list"),
            TodoChildCommand::List
        ));
        assert_eq!(
            parse_todo_child_command("list extra").expect_err("list tail must fail"),
            super::TODO_LIST_USAGE
        );
        assert!(matches!(
            parse_todo_child_command("NEW").expect("bare new"),
            TodoChildCommand::New { label: None }
        ));
        assert!(matches!(
            parse_todo_child_command("new durable worker").expect("labeled new"),
            TodoChildCommand::New { label: Some(label) } if label == "durable worker"
        ));
        assert!(matches!(
            parse_todo_child_command("select child-1").expect("one required id"),
            TodoChildCommand::Select { child_session_id } if child_session_id == "child-1"
        ));
        assert!(matches!(
            parse_todo_child_command("remove child-1").expect("remove required id"),
            TodoChildCommand::Remove { child_session_id } if child_session_id == "child-1"
        ));
        for argument in [
            "select",
            "select child-1 extra",
            "archive",
            "archive child-1 extra",
            "remove",
            "remove child-1 extra",
        ] {
            assert!(parse_todo_child_command(argument).is_err(), "{argument}");
        }
        assert!(matches!(
            parse_todo_child_command("status").expect("selected child status"),
            TodoChildCommand::Status {
                child_session_id: None
            }
        ));
        assert!(matches!(
            parse_todo_child_command("PAUSE child-1").expect("one optional id"),
            TodoChildCommand::Pause { child_session_id: Some(child_session_id) } if child_session_id == "child-1"
        ));
        for argument in [
            "status child-1 extra",
            "pause child-1 extra",
            "resume child-1 extra",
            "stop child-1 extra",
            "cancel child-1 extra",
        ] {
            assert!(parse_todo_child_command(argument).is_err(), "{argument}");
        }
    }

    #[test]
    fn unified_todo_parser_preserves_every_command_payload_and_exact_usage() {
        assert!(matches!(
            parse_todo_command("").expect("bare open"),
            TodoCommand::Open
        ));
        assert!(matches!(
            parse_todo_command("LIST").expect("list open"),
            TodoCommand::Open
        ));
        assert_eq!(
            parse_todo_command("list extra").expect_err("list tail"),
            super::TODO_LIST_USAGE
        );

        let explicit_id = "00000000-0000-4000-8000-000000000001";
        assert!(matches!(
            parse_todo_command(&format!("SEND {explicit_id} keep exact words")).expect("send"),
            TodoCommand::Child(TodoChildCommand::Send { child_session_id: Some(child_session_id), message })
                if child_session_id == explicit_id && message == "keep exact words"
        ));
        assert!(matches!(
            parse_todo_command("add child-1 --revision 7 --depends a,b --agent worker --role writer implement feature").expect("add"),
            TodoCommand::Child(TodoChildCommand::Add {
                child_session_id,
                content,
                depends_on,
                agent: Some(agent),
                role: Some(role),
                expected_revision: Some(7),
            }) if child_session_id == "child-1"
                && content == "implement feature"
                && depends_on == ["a", "b"]
                && agent == "worker"
                && role == "writer"
        ));
        assert!(matches!(
            parse_todo_command("START exact task text").expect("start"),
            TodoCommand::Dag(TodoOp::Start { task }) if task == "exact task text"
        ));
        assert!(matches!(
            parse_todo_command("DONE exact task text").expect("done"),
            TodoCommand::Dag(TodoOp::Done { task: Some(task), phase: None }) if task == "exact task text"
        ));
        assert!(matches!(
            parse_todo_command("DROP exact task text").expect("drop"),
            TodoCommand::Dag(TodoOp::Drop { task: Some(task), phase: None }) if task == "exact task text"
        ));
        assert!(matches!(
            parse_todo_command("CLEAR").expect("clear"),
            TodoCommand::Dag(TodoOp::Rm {
                task: None,
                phase: None,
                cascade: true
            })
        ));
        assert!(matches!(
            parse_todo_command("REMOVE child-1").expect("unified remove"),
            TodoCommand::Child(TodoChildCommand::Remove { child_session_id })
                if child_session_id == "child-1"
        ));
        assert!(matches!(
            parse_todo_command("# Plan\n- [ ] work").expect("markdown"),
            TodoCommand::Markdown(markdown) if markdown == "# Plan\n- [ ] work"
        ));
        for (argument, usage) in [
            ("select", super::TODO_SELECT_USAGE),
            ("select child extra", super::TODO_SELECT_USAGE),
            ("send", super::TODO_SEND_USAGE),
            ("add child", super::TODO_ADD_USAGE),
            ("status child extra", super::TODO_STATUS_USAGE),
            ("stop child extra", super::TODO_STOP_USAGE),
            ("pause child extra", super::TODO_PAUSE_USAGE),
            ("resume child extra", super::TODO_RESUME_USAGE),
            ("cancel child extra", super::TODO_CANCEL_USAGE),
            ("archive", super::TODO_ARCHIVE_USAGE),
            ("archive child extra", super::TODO_ARCHIVE_USAGE),
            ("remove", super::TODO_REMOVE_USAGE),
            ("remove child extra", super::TODO_REMOVE_USAGE),
            ("start", super::TODO_START_USAGE),
            ("done", super::TODO_DONE_USAGE),
            ("drop", super::TODO_DROP_USAGE),
            ("clear extra", super::TODO_CLEAR_USAGE),
        ] {
            assert_eq!(
                parse_todo_command(argument).expect_err(argument),
                usage,
                "{argument}"
            );
        }
        assert_eq!(
            parse_todo_command("not markdown").expect_err("unknown"),
            super::TODO_COMMAND_USAGE
        );
    }
}
