//! Interactive line REPL and slash-command dispatch.
//!
//! Every prompt turn runs through `Application::prompt` and consumes the same
//! human event renderer as print mode. Ctrl-C aborts the live application turn
//! without leaving the REPL; Ctrl-D (EOF) exits.

use std::io::{IsTerminal as _, Write as _};
use std::path::{Path, PathBuf};

use anyhow::Result;
use tokio::io::AsyncBufReadExt;

use pi_coding::{Application, ApplicationEvent, Session, TrustDecision};

use crate::commands::{list_models, list_sessions, resolve_model_spec};
use crate::resume_catalog::{
    ResumeCatalogRequest, ResumeSelectionRequest, load_resume_catalog, switch_resume_selection,
};
use crate::output::{error_line, parse_thinking_level, thinking_level_str};

/// Run the interactive REPL over an already-built [`Application`].
pub async fn interactive(application: Application, initial_prompts: Vec<String>) -> Result<()> {
    let session = application.session();
    print_header(&session);
    println!("Type your message. /help for commands, Ctrl-D or /quit to exit.");

    let stdin = tokio::io::stdin();
    let mut reader = tokio::io::BufReader::new(stdin);
    let mut line = String::new();
    let ansi = std::io::stdout().is_terminal();
    for prompt in initial_prompts {
        if let Err(error) = run_turn(&application, &prompt).await {
            error_line(&format!("{error:#}"));
        }
        println!();
    }

    loop {
        line.clear();
        if ansi {
            print!("\n\x1b[1m> \x1b[0m");
        } else {
            print!("\n> ");
        }
        let _ = std::io::stdout().flush();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            // EOF (Ctrl-D): exit cleanly.
            println!();
            return Ok(());
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix('/') {
            if handle_slash(&application, rest).await? {
                return Ok(());
            }
            continue;
        }
        let result = if let Some((command, exclude_from_context)) = parse_bash_command(trimmed) {
            run_bash(&application, command, exclude_from_context).await
        } else {
            run_turn(&application, trimmed).await
        };
        if let Err(error) = result {
            error_line(&format!("{error}"));
        }
        println!();
    }
}

fn print_header(session: &Session) {
    let (provider, id) = session
        .model()
        .map(|m| (m.provider, m.id))
        .unwrap_or_else(|| ("?".to_string(), "?".to_string()));
    if std::io::stdout().is_terminal() {
        println!(
            "\x1b[1mpi (rs)\x1b[0m · {provider}/{id} · cwd {}",
            session.cwd().display()
        );
    } else {
        println!("pi (rs) · {provider}/{id} · cwd {}", session.cwd().display());
    }
}

async fn resume_application(application: &Application, input: &str) -> Result<PathBuf> {
    let catalog = pi_coding::SessionCatalog::from_env().map_err(anyhow::Error::new)?;
    let cwd = application.session().cwd().to_path_buf();
    let result = switch_resume_selection(
        application,
        &catalog,
        &ResumeSelectionRequest::Input(input.to_owned()),
        Some(&cwd),
    )
    .await?;
    Ok(result.path)
}

/// Dispatch a slash command (without the leading `/`). Returns `true` to quit.
async fn handle_slash(application: &Application, line: &str) -> Result<bool> {
    let session = application.session();
    let mut parts = line.split_whitespace();
    let cmd = parts.next().unwrap_or("");
    let arg = line[cmd.len()..].trim();
    match cmd {
        "quit" | "exit" => return Ok(true),
        "help" => {
            let ansi = std::io::stdout().is_terminal();
            let (commands, diagnostics) = crate::interactive_commands::executable_catalog(application);
            for command in commands {
                let usage = crate::interactive_commands::builtin(&command.name)
                    .map(crate::interactive_commands::usage)
                    .unwrap_or_else(|| format!("/{}", command.name));
                if ansi {
                    println!("  \x1b[1m{usage:<24}\x1b[0m {}", command.description);
                } else {
                    println!("  {usage:<24} {}", command.description);
                }
            }
            for diagnostic in diagnostics {
                error_line(&diagnostic);
            }
        }
        "settings" => match crate::interactive_commands::parse_interactive_settings_command(
            cmd,
            (!arg.is_empty()).then_some(arg),
        ) {
            Ok(Some(command)) => match crate::interactive_commands::execute_interactive_settings_command(
                application,
                command,
            )
            .await
            {
                Ok(output) => println!("{output}"),
                Err(error) => error_line(&format!("settings command failed: {error:#}")),
            },
            Ok(None) => unreachable!("settings command name was matched"),
            Err(error) => error_line(&format!("{error:#}")),
        },
        "scoped-models" => error_line("/scoped-models requires the full-screen TUI; use --models for line-oriented sessions"),
        "changelog" => println!("{}", include_str!("../../../CHANGELOG.md")),
        "hotkeys" => println!("Enter submit · Ctrl-D quit · Ctrl-C abort · !command record bash · !!command exclude bash from context"),
        "theme" => error_line("/theme requires the full-screen TUI"),
        "login" => match crate::auth_commands::login(
            (!arg.is_empty()).then_some(arg),
            std::io::IsTerminal::is_terminal(&std::io::stdin()),
        )
        .await
        {
            Ok(info) => println!(
                "logged in to {} using {}",
                info.provider_id,
                info.credential_type.label()
            ),
            Err(error) => error_line(&format!("{error:#}")),
        },
        "logout" => match crate::auth_commands::logout(
            (!arg.is_empty()).then_some(arg),
            std::io::IsTerminal::is_terminal(&std::io::stdin()),
        )
        .await
        {
            Ok(info) => println!("logged out of {}", info.provider_id),
            Err(error) => error_line(&format!("{error:#}")),
        },
        "models" => list_models(parts.next()).await?,
        "model" if arg.is_empty() => match application.state().await.model {
            Some(model) => println!("current: {}/{}", model.provider, model.id),
            None => println!("no model selected"),
        },
        "model" => match resolve_model_spec(arg) {
            Ok((model, _)) => {
                let reference = format!("{}/{}", model.provider, model.id);
                match application.set_model_with_resolved_auth(model).await {
                    Ok(change) => {
                        if change.clamped {
                            println!("switched to {reference}");
                            println!("{}", change.message);
                        } else {
                            println!("switched to {reference}");
                        }
                    }
                    Err(error) => error_line(&format!("{error:#}")),
                }
            }
            Err(error) => error_line(&format!("{error:#}")),
        },
        "think" | "thinking" => {
            let level = parse_thinking_level(arg);
            let change = application.set_thinking_level(level);
            println!("{}", change.message);
        },
        "new" => {
            application.new_session().await?;
            println!("started a new transcript");
        },
        "name" if arg.is_empty() => println!(
            "{}",
            application
                .state()
                .await
                .session_name
                .unwrap_or_else(|| "(unnamed)".to_owned())
        ),
        "name" => {
            application.set_session_name(arg)?;
            println!("session name: {arg}");
        }
        "session" => {
            let state = application.state().await;
            println!(
                "id {} · {} messages · {}",
                state.session_id.as_deref().unwrap_or("(not recording)"),
                state.message_count,
                state.session_file.as_deref().unwrap_or("in memory")
            );
        }
        "sessions" => list_sessions(session.cwd())?,
        "resume" if arg.is_empty() => match pi_coding::SessionCatalog::from_env() {
            Ok(catalog) => match load_resume_catalog(
                &catalog,
                &ResumeCatalogRequest {
                    cwd_scope: Some(session.cwd().to_path_buf()),
                    ..ResumeCatalogRequest::default()
                },
            ) {
                Ok(result) if result.rows.is_empty() => println!("No sessions for this directory."),
                Ok(result) => {
                    for row in result.rows {
                        let imported = if matches!(
                            row.status,
                            pi_coding::CatalogRowStatus::AlreadyImported { .. }
                        ) {
                            " imported"
                        } else {
                            ""
                        };
                        println!(
                            "[{:<10}] {}  {}  {}{}  {}",
                            row.source_badge,
                            row.display_time,
                            row.session_id,
                            row.summary,
                            imported,
                            row.cwd.display()
                        );
                    }
                }
                Err(error) => error_line(&format!("failed to list resumable sessions: {error}")),
            },
            Err(error) => error_line(&format!("failed to open session catalog: {error}")),
        },
        "resume" => {
            match resume_application(application, arg).await {
                Ok(path) => println!("resumed {}", path.display()),
                Err(error) => error_line(&format!("{error:#}")),
            }
        }
        "import" => {
            if arg.is_empty() {
                error_line("usage: /import <path.jsonl>");
            } else {
                let imported =
                    pi_coding::import_session(pi_coding::SourceSessionFormat::Pi, Path::new(arg))?;
                application.switch_session(&imported.path).await?;
                println!("imported and resumed {}", imported.path.display());
            }
        }
        "compact" => {
            let result = application
                .compact((!arg.is_empty()).then_some(arg))
                .await?;
            println!(
                "compacted {} -> {} estimated tokens",
                result.tokens_before,
                result.estimated_tokens_after.unwrap_or_default()
            );
        }
        "fork" if arg.is_empty() => {
            for message in application.fork_messages()? {
                println!(
                    "{}  {}",
                    message.entry_id,
                    message.text.lines().next().unwrap_or_default()
                );
            }
        }
        "fork" => {
            let text = application.fork_session(arg).await?;
            println!("forked from {arg}\n{text}");
        }
        "clone" => {
            application.clone_session().await?;
            println!("cloned current session branch");
        }
        "tree" => println!(
            "{}",
            serde_json::to_string_pretty(&application.session_tree()?)?
        ),
        "loop" | "loops" | "loop-update" | "loop-delete" | "loop-cancel" => {
            match crate::loop_commands::parse_interactive_loop_command(
                cmd,
                (!arg.is_empty()).then_some(arg),
            ) {
                Ok(Some(command)) => {
                    match crate::loop_commands::execute_interactive_loop_command(
                        application,
                        command,
                    )
                    .await
                    {
                        Ok(output) => println!("{output}"),
                        Err(error) => error_line(&format!("{error:#}")),
                    }
                }
                Ok(None) => unreachable!("loop command name was matched"),
                Err(error) => error_line(&format!("{error:#}")),
            }
        }
        "goal" => match crate::goal_commands::parse_interactive_goal_command((!arg.is_empty()).then_some(arg)) {
            Ok(command) => match crate::goal_commands::execute_interactive_goal_command(application, command) {
                Ok(output) => println!("{output}"),
                Err(error) => error_line(&format!("{error:#}")),
            },
            Err(error) => error_line(&format!("{error:#}")),
        },
        "todo" => {
            if arg.is_empty() {
                let markdown = pi_coding::todo_phases_to_markdown(&application.todo_state().phases);
                println!("{markdown}");
            } else {
                match pi_coding::parse_todo_markdown(arg) {
                    Ok(phases) => match application.set_todos(phases) {
                        Ok(result) => println!("updated todo list: {}", result.summary),
                        Err(error) => error_line(&format!("failed to set todos: {error:#}")),
                    },
                    Err(error) => error_line(&format!("invalid todo markdown: {error:#}")),
                }
            }
        }
        "reload" => {
            let result = application.reload().await?;
            println!("reloaded resource generation {}", result.generation);
        }
        "trust" => {
            let decision = match arg {
                "trusted" | "trust" | "yes" => TrustDecision::Trusted,
                "untrusted" | "no" => TrustDecision::Untrusted,
                "ask" | "clear" => TrustDecision::Ask,
                _ => {
                    error_line("usage: /trust <trusted|untrusted|ask>");
                    return Ok(false);
                }
            };
            let result = application.set_project_trust(decision).await?;
            println!(
                "updated project trust; reloaded generation {}",
                result.generation
            );
        }
        "export" => {
            let output = (!arg.is_empty()).then(|| PathBuf::from(arg));
            let path = if output.as_ref().is_some_and(|path| {
                path.extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("jsonl"))
            }) {
                application.export_jsonl(output.as_deref())?
            } else {
                application.export_html(output.as_deref())?
            };
            println!("{}", path.display());
        }
        "share" => {
            let mut events = application.subscribe();
            application.share_session();
            loop {
                match events.recv().await? {
                    ApplicationEvent::ShareSucceeded { url } => {
                        println!("{url}");
                        break;
                    }
                    ApplicationEvent::ShareFailed { message } => {
                        error_line(&message);
                        break;
                    }
                    _ => {}
                }
            }
        }
        "copy" => println!("{}", application.last_assistant_text().unwrap_or_default()),
        "llama" => match crate::llama_commands::run_slash(arg).await {
            Ok(message) => println!("{message}"),
            Err(error) => error_line(&format!("{error:#}")),
        },
        "run" => match crate::interactive_commands::parse_run_invocation(arg) {
            Ok((command, arguments)) => {
                match crate::interactive_commands::invoke_extension_command(
                    application,
                    command,
                    arguments.to_owned(),
                )
                .await
                {
                    Ok(value) if !value.is_null() => println!("{value}"),
                    Ok(_) => println!("ran /{command}"),
                    Err(error) => error_line(&format!("/run {command} failed: {error:#}")),
                }
            }
            Err(error) => error_line(&format!("{error:#}")),
        },
        "chain" | "run-chain" => match crate::interactive_commands::parse_chain_invocation(arg) {
            Ok(steps) => {
                match crate::interactive_commands::invoke_extension_chain(application, &steps).await
                {
                    Ok(outputs) => {
                        for (name, value) in outputs {
                            if value.is_null() {
                                println!("/{name}: ok");
                            } else {
                                println!("/{name}: {value}");
                            }
                        }
                    }
                    Err(error) => error_line(&format!("/{cmd} failed: {error:#}")),
                }
            }
            Err(error) => error_line(&format!("{error:#}")),
        },
        "ps" | "process" => match crate::process_commands::parse_interactive_process_command(cmd, (!arg.is_empty()).then_some(arg)) {
            Ok(Some(command)) => match crate::process_commands::execute_interactive_process_command(application, command).await {
                Ok(output) => println!("{output}"),
                Err(error) => error_line(&format!("{error:#}")),
            },
            Ok(None) => unreachable!("matched process command"),
            Err(error) => error_line(&format!("{error:#}")),
        }
        other => {
            let (commands, _) = crate::interactive_commands::executable_catalog(application);
            match commands.iter().find(|command| command.name == other).map(|command| command.source) {
                Some(crate::interactive_commands::CommandSource::Prompt | crate::interactive_commands::CommandSource::Skill) => {
                    match crate::interactive_commands::expand_resource_command(application, other, arg) {
                        Ok(Some(expanded)) => run_turn(application, &expanded).await?,
                        Ok(None) => error_line(&format!("command /{other} is no longer available; try /reload")),
                        Err(error) => error_line(&format!("failed to expand /{other}: {error:#}")),
                    }
                }
                Some(crate::interactive_commands::CommandSource::Extension) => {
                    let result = match application.extension_runtime() {
                        Some(runtime) => runtime.invoke_command(other, arg.to_owned(), None, None).await,
                        None => Err(anyhow::anyhow!("extension runtime is not loaded")),
                    };
                    match result {
                        Ok(value) if !value.is_null() => println!("{value}"),
                        Ok(_) => println!("ran /{other}"),
                        Err(error) => error_line(&format!("extension command /{other} failed: {error:#}")),
                    }
                }
                Some(crate::interactive_commands::CommandSource::Builtin) => {
                    let usage = crate::interactive_commands::builtin(other)
                        .map(crate::interactive_commands::usage)
                        .unwrap_or_else(|| format!("/{other}"));
                    error_line(&format!("{usage} is unavailable in line-oriented mode"));
                }
                None => {
                    let suggestion = crate::interactive_commands::closest_builtin(other)
                        .map_or_else(String::new, |name| format!("; did you mean /{name}?"));
                    error_line(&format!("unknown command \"/{other}\"{suggestion} (try /help)"));
                }
            }
        }
    }
    Ok(false)
}

fn parse_bash_command(input: &str) -> Option<(&str, bool)> {
    let command = input.strip_prefix('!')?;
    let (command, exclude_from_context) = command
        .strip_prefix('!')
        .map_or((command, false), |command| (command, true));
    Some((command.trim_start(), exclude_from_context))
}

async fn run_bash(
    application: &Application,
    command: &str,
    exclude_from_context: bool,
) -> Result<()> {
    if command.is_empty() {
        return Ok(());
    }
    let mut stdout = std::io::stdout();
    let ansi = stdout.is_terminal();
    crate::human_event_renderer::run_human_bash_to(
        application,
        command,
        exclude_from_context,
        &mut stdout,
        ansi,
    )
    .await
}

/// Run one prompt turn through the shared application event renderer.
pub async fn run_turn_to<W>(application: &Application, prompt: &str, writer: &mut W) -> Result<()>
where
    W: std::io::Write,
{
    crate::human_event_renderer::run_human_turn_to(application, prompt, writer, false)
        .await
        .map(|_| ())
}

async fn run_turn(application: &Application, prompt: &str) -> Result<()> {
    let mut stdout = std::io::stdout();
    let ansi = stdout.is_terminal();
    crate::human_event_renderer::run_human_turn_to(application, prompt, &mut stdout, ansi)
        .await
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::parse_bash_command;

    #[test]
    fn parses_recorded_and_context_excluded_bash() {
        assert_eq!(parse_bash_command("!echo kept"), Some(("echo kept", false)));
        assert_eq!(
            parse_bash_command("!!echo hidden"),
            Some(("echo hidden", true))
        );
        assert_eq!(parse_bash_command("hello"), None);
    }
}
