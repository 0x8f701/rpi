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

use crate::commands::{list_models, list_sessions_in, resolve_model_spec};
use crate::resume_catalog::{
    ResumeCatalogRequest, ResumeSelectionRequest, ResumeSelectionResult, effective_resume_sources,
    load_resume_catalog, switch_resume_selection,
};
use crate::output::{error_line, parse_thinking_level, thinking_level_str};


/// Run the external editor for `/persona new`/`/persona edit` (line REPL: the
/// editor runs in the foreground — the REPL owns no alternate screen), then
/// validate and atomically commit the result.
async fn run_repl_persona_editor(
    application: &Application,
    name: &str,
    kind: crate::interactive_commands::PersonaEditKind,
) -> Result<String> {
    use crate::interactive_commands::{
        commit_persona_definition, persona_editor_command, persona_editor_seed, spawn_editor_on,
    };
    let seed = persona_editor_seed(application, name, kind)?;
    let editor = persona_editor_command(application);
    let dir = std::env::temp_dir().join(format!("pi-persona-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&dir)
        .map_err(|e| anyhow::anyhow!("creating persona editor workspace: {e}"))?;
    let path = dir.join("persona.md");
    if let Err(e) = std::fs::write(&path, &seed) {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(anyhow::anyhow!("writing persona editor seed: {e}"));
    }
    let result = spawn_editor_on(&editor, &path).await;
    let content = match result {
        Ok(()) => std::fs::read_to_string(&path)
            .map_err(|e| anyhow::anyhow!("reading persona editor result: {e}")),
        Err(error) => Err(error),
    };
    let _ = std::fs::remove_dir_all(&dir);
    let content = content?;
    commit_persona_definition(application, name, &content, kind).await
}
/// Run the interactive REPL over an already-built [`Application`].
pub async fn interactive(
    application: Application,
    initial_prompts: Vec<String>,
    collab_host: Option<crate::collab_commands::CollabHost>,
) -> Result<()> {
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
    // A joined collab room turns this process into a guest console: plain
    // lines are forwarded to the host session instead of running locally.
    let mut collab_guest: Option<crate::collab_guest::CollabGuestHandle> = None;

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
            if handle_slash(&application, &mut collab_guest, collab_host.as_ref(), rest).await? {
                return Ok(());
            }
            continue;
        }
        if let Some(guest) = collab_guest.as_ref() {
            // Guest console mode: plain lines go to the host session.
            if guest.is_writable() {
                match guest.prompt(trimmed.to_owned()).await {
                    Ok(()) => {}
                    Err(error) => error_line(&format!("{error:#}")),
                }
            } else {
                println!("[collab] prompt rejected: view-only guests cannot write");
            }
            println!();
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
            "\x1b[1mrpi\x1b[0m · {provider}/{id} · cwd {}",
            session.cwd().display()
        );
    } else {
        println!("rpi · {provider}/{id} · cwd {}", session.cwd().display());
    }
}

async fn resume_application(application: &Application, input: &str) -> Result<ResumeSelectionResult> {
    let session = application.session();
    let catalog = pi_coding::SessionCatalog::from_env()
        .map_err(anyhow::Error::new)?
        .with_native_session_root(session.session_dir());
    let cwd = session.cwd().to_path_buf();
    let sources = effective_resume_sources(application);
    switch_resume_selection(
        application,
        &catalog,
        &ResumeSelectionRequest::Input(input.to_owned()),
        Some(&cwd),
        &sources,
    )
    .await
}

/// Dispatch a slash command (without the leading `/`). Returns `true` to quit.
async fn handle_slash(
    application: &Application,
    collab_guest: &mut Option<crate::collab_guest::CollabGuestHandle>,
    collab_host: Option<&crate::collab_commands::CollabHost>,
    line: &str,
) -> Result<bool> {
    let session = application.session();
    let mut parts = line.split_whitespace();
    let cmd = parts.next().unwrap_or("");
    let arg = line[cmd.len()..].trim();
    match cmd {
        "quit" | "exit" => return Ok(true),
        "help" => {
            let ansi = std::io::stdout().is_terminal();
            for command in crate::interactive_commands::visible_catalog() {
                let usage = crate::interactive_commands::builtin(&command.name)
                    .map(crate::interactive_commands::usage)
                    .unwrap_or_else(|| format!("/{}", command.name));
                if ansi {
                    println!("  \x1b[1m{usage:<24}\x1b[0m {}", command.description);
                } else {
                    println!("  {usage:<24} {}", command.description);
                }
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
        "queue" => {
            let (steering, follow_up) = application.queued_messages().await;
            let total = steering.len() + follow_up.len();
            match arg {
                "" => {
                    if total == 0 {
                        println!("Queue is empty");
                    } else {
                        println!(
                            "Pending prompts: {} steering, {} follow-up (/queue cancel clears them)",
                            steering.len(),
                            follow_up.len()
                        );
                        for (kind, messages) in [("steering", steering), ("follow-up", follow_up)] {
                            for message in messages {
                                let pi_ai::Message::User(user) = message else {
                                    continue;
                                };
                                let text = user
                                    .content
                                    .iter()
                                    .filter_map(|block| match block {
                                        pi_ai::ContentBlock::Text { text, .. } => {
                                            Some(text.as_str())
                                        }
                                        _ => None,
                                    })
                                    .collect::<Vec<_>>()
                                    .join(" ")
                                    .trim()
                                    .to_owned();
                                if !text.is_empty() {
                                    println!("  {kind}: {text}");
                                }
                            }
                        }
                    }
                }
                "cancel" => {
                    if total == 0 {
                        println!("Queue is empty");
                    } else {
                        application.drain_queued_messages().await;
                        println!(
                            "Cancelled {total} queued prompt{}",
                            if total == 1 { "" } else { "s" }
                        );
                    }
                }
                other => error_line(&format!("/queue [cancel] (unknown action {other:?})")),
            }
        }
        "changelog" => println!("{}", include_str!("../../../CHANGELOG.md")),
        "hotkeys" => println!("Enter submit · Ctrl-D quit · Ctrl-C abort · !command record bash · !!command exclude bash from context"),
        "theme" => error_line("/theme requires the full-screen TUI"),
        "login" => match crate::auth_commands::login(
            (!arg.is_empty()).then_some(arg),
            None,
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
            None,
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
        "new" | "fresh" => {
            let outcome = application.new_session().await?;
            if !outcome.cancelled {
                if let Err(error) =
                    crate::session_run::rebind_workflows_for_active_session(application).await
                {
                    error_line(&format!("workflow storage rebind failed: {error:#}"));
                }
                println!("started a new transcript");
            } else {
                println!("new session cancelled");
            }
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
        "handoff" => match run_handoff(&application, arg).await {
            Ok(text) => {
                println!("{text}");
                match crate::clipboard::write_text(&text).await {
                    Ok(()) => println!("(copied to clipboard)"),
                    Err(_) => {}
                }
            }
            Err(error) => error_line(&format!("{error:#}")),
        },
        "sessions" => list_sessions_in(session.cwd(), &session.session_dir())?,
        "resume" if arg.is_empty() => {
            let catalog = pi_coding::SessionCatalog::from_env()
                .map(|catalog| catalog.with_native_session_root(session.session_dir()));
            let sources = effective_resume_sources(application);
            match catalog {
                Ok(catalog) => match load_resume_catalog(
                    &catalog,
                    &ResumeCatalogRequest {
                        cwd_scope: Some(session.cwd().to_path_buf()),
                        sources,
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
            }
        }
        "resume" => {
            match resume_application(application, arg).await {
                Ok(result) if result.cancelled => println!("session resume cancelled"),
                Ok(result) => println!("resumed {}", result.path.display()),
                Err(error) => error_line(&format!("{error:#}")),
            }
        }
        "import" => {
            if arg.is_empty() {
                error_line("usage: /import <path.jsonl>");
            } else {
                let session_dir = session.session_dir();
                std::fs::create_dir_all(&session_dir)?;
                let imported = pi_coding::import_session_to(
                    pi_coding::SourceSessionFormat::Pi,
                    Path::new(arg),
                    &session_dir,
                )?;
                let outcome = application.switch_session(&imported.path).await?;
                if !outcome.cancelled {
                    if let Err(error) =
                        crate::session_run::rebind_workflows_for_active_session(application).await
                    {
                        error_line(&format!("workflow storage rebind failed: {error:#}"));
                    }
                    println!("imported and resumed {}", imported.path.display());
                } else {
                    println!("session resume cancelled");
                }
            }
        }
        "compact" => {
            let (snap, instructions) = crate::interactive_commands::parse_compact_arguments(arg);
            let result = if snap {
                application.compact_snap().await?
            } else {
                application.compact(instructions.as_deref()).await?
            };
            println!(
                "compacted {} -> {} estimated tokens",
                result.tokens_before,
                result.estimated_tokens_after.unwrap_or_default()
            );
        }
        "snapcompact" => {
            let result = application.compact_snap().await?;
            println!(
                "compacted {} -> {} estimated tokens",
                result.tokens_before,
                result.estimated_tokens_after.unwrap_or_default()
            );
        }
        "rewind" => {
            let invocation = crate::interactive_commands::parse_rewind_invocation(
                (!arg.is_empty()).then_some(arg),
            );
            match crate::interactive_commands::execute_rewind(application, invocation).await {
                Ok(output) => println!("{output}"),
                Err(error) => error_line(&format!("rewind failed: {error:#}")),
            }
        }
        "checkpoint" => {
            if arg.is_empty() {
                error_line("Usage: /checkpoint <name>");
            } else {
                match crate::interactive_commands::execute_checkpoint(application, arg) {
                    Ok(output) => println!("{output}"),
                    Err(error) => error_line(&format!("checkpoint failed: {error:#}")),
                }
            }
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
            let outcome = application.fork_session(arg).await?;
            if outcome.cancelled {
                println!("session fork cancelled");
            } else {
                if let Err(error) =
                    crate::session_run::rebind_workflows_for_active_session(application).await
                {
                    error_line(&format!("workflow storage rebind failed: {error:#}"));
                }
                println!("forked from {arg}\n{}", outcome.text);
            }
        }
        "clone" => {
            let outcome = application.clone_session().await?;
            if !outcome.cancelled {
                if let Err(error) =
                    crate::session_run::rebind_workflows_for_active_session(application).await
                {
                    error_line(&format!("workflow storage rebind failed: {error:#}"));
                }
                println!("cloned current session branch");
            } else {
                println!("session clone cancelled");
            }
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
            Ok(command) => match crate::goal_commands::execute_interactive_goal_command(application, command).await {
                Ok(output) => println!("{output}"),
                Err(error) => error_line(&format!("{error:#}")),
            },
            Err(error) => error_line(&format!("{error:#}")),
        },
        "workflow" => {
            match crate::workflow_commands::parse_interactive_workflow_command(
                (!arg.is_empty()).then_some(arg),
            ) {
                Ok(command) => {
                    match crate::workflow_commands::execute_interactive_workflow_on_application(
                        application,
                        command,
                    )
                    .await
                    {
                        Ok(crate::workflow_commands::WorkflowCommandEffect::OpenPage) => {
                            println!(
                                "Open the workflows page in the full-screen TUI (bare /workflow)."
                            );
                        }
                        Ok(crate::workflow_commands::WorkflowCommandEffect::Message(output)) => {
                            println!("{output}");
                        }
                        Err(error) => error_line(&format!("{error:#}")),
                    }
                }
                Err(error) => error_line(&format!("{error:#}")),
            }
        }
        "skill" => {
            if arg.is_empty() {
                error_line("usage: /skill <name>");
                return Ok(false);
            }
            match crate::interactive_commands::skill_frontmatter_summary(application, arg) {
                Some(summary) => println!("{summary}"),
                None => error_line(&format!("unknown skill {arg:?}")),
            }
        }
        "role" => {
            match crate::interactive_commands::parse_interactive_role_command(
                (!arg.is_empty()).then_some(arg),
            ) {
                Ok(command) => {
                    match crate::interactive_commands::execute_interactive_role_command(
                        application,
                        command,
                    ) {
                        Ok(output) => println!("{output}"),
                        Err(error) => error_line(&format!("{error:#}")),
                    }
                }
                Err(error) => error_line(&format!("{error:#}")),
            }
        }
        "persona" => {
            match crate::interactive_commands::parse_interactive_persona_command(
                (!arg.is_empty()).then_some(arg),
            ) {
                Ok(command) => {
                    let result = match command {
                        crate::interactive_commands::InteractivePersonaCommand::New { name } => {
                            run_repl_persona_editor(
                                application,
                                &name,
                                crate::interactive_commands::PersonaEditKind::New,
                            )
                            .await
                        }
                        crate::interactive_commands::InteractivePersonaCommand::Edit { name } => {
                            run_repl_persona_editor(
                                application,
                                &name,
                                crate::interactive_commands::PersonaEditKind::Edit,
                            )
                            .await
                        }
                        other => {
                            crate::interactive_commands::execute_interactive_persona_command(
                                application,
                                other,
                            )
                            .await
                        }
                    };
                    match result {
                        Ok(output) => println!("{output}"),
                        Err(error) => error_line(&format!("{error:#}")),
                    }
                }
                Err(error) => error_line(&format!("{error:#}")),
            }
        }
        "todo" => {
            if arg.is_empty() || arg.eq_ignore_ascii_case("list") {
                let text = crate::tui::format_todo_human_lines(&application.todo_state().phases);
                println!("{text}");
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
        "dump" => {
            let request = crate::interactive_commands::parse_dump_invocation(arg);
            match crate::interactive_commands::execute_dump(application, request).await {
                Ok(path) => println!("{}", path.display()),
                Err(error) => error_line(&format!("dump failed: {error:#}")),
            }
        }
        "share" => {
            match crate::interactive_commands::parse_share_invocation(arg) {
                Ok(request) if request.encrypt => {
                    let passphrase = match request.passphrase {
                        Some(passphrase) => passphrase,
                        None => match crate::interactive_commands::prompt_passphrase(
                            "Enter share passphrase",
                        ) {
                            Ok(passphrase) => passphrase,
                            Err(error) => {
                                error_line(&format!("encrypted share cancelled: {error:#}"));
                                return Ok(false);
                            }
                        },
                    };
                    match crate::interactive_commands::execute_encrypted_share(
                        application,
                        &passphrase,
                    )
                    .await
                    {
                        Ok(message) => println!("{message}"),
                        Err(error) => error_line(&format!("encrypted share failed: {error:#}")),
                    }
                }
                Ok(_) => {
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
                Err(error) => error_line(&format!("{error:#}")),
            }
        }
        "copy" => println!("{}", application.last_assistant_text().unwrap_or_default()),
        "llama" => match crate::llama_commands::run_slash(arg).await {
            Ok(message) => println!("{message}"),
            Err(error) => error_line(&format!("{error:#}")),
        },
        "collab" => {
            let invocation = match crate::interactive_commands::parse_collab_invocation(arg) {
                Ok(invocation) => invocation,
                Err(error) => {
                    error_line(&format!("{error:#}"));
                    return Ok(false);
                }
            };
            match crate::collab_commands::execute(collab_host, invocation).await {
                Ok(output) => println!("{output}"),
                Err(error) => error_line(&format!("{error:#}")),
            }
        }
        "join" => {
            let link = match crate::interactive_commands::parse_join_invocation(arg) {
                Ok(link) => link,
                Err(error) => {
                    error_line(&format!("{error:#}"));
                    return Ok(false);
                }
            };
            if collab_guest.is_some() {
                error_line("already joined a collab room; /leave first");
                return Ok(false);
            }
            let original = arg.to_owned();
            let sink = Box::<crate::collab_guest::PrintingGuestSink>::default();
            let handle = crate::collab_guest::spawn_guest(link, original, sink);
            println!(
                "[collab] joined room {} as {}",
                handle.room_id,
                if handle.is_writable() { "control" } else { "view-only" }
            );
            *collab_guest = Some(handle);
        }
        "leave" => {
            match collab_guest.take() {
                Some(guest) => match guest.leave().await {
                    Ok(()) => println!("[collab] left the room"),
                    Err(error) => error_line(&format!("[collab] leave failed: {error:#}")),
                },
                None => error_line("not joined to any collab room"),
            }
        }
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

/// Builds the `/handoff` block for the line REPL. Bare `/handoff` renders the
/// deterministic envelope; `--prose` adds one bounded summarizer paragraph
/// (the REPL dispatch is sequential, so awaiting the single
/// [`pi_coding::HANDOFF_SUMMARIZE_TIMEOUT`]-bounded provider call is safe).
async fn run_handoff(application: &Application, argument: &str) -> Result<String> {
    use crate::interactive_commands::{parse_handoff_invocation, HandoffInvocation};
    let handoff = match parse_handoff_invocation(argument)? {
        HandoffInvocation::Envelope => application.generate_handoff(),
        HandoffInvocation::Prose => application.generate_handoff_with_prose().await?,
    };
    Ok(handoff.render())
}

#[cfg(test)]
mod tests {
    use super::{parse_bash_command, run_handoff};
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use pi_ai::{
        new_assistant_message_event_stream, AssistantMessage, ContentBlock, Model, StopReason,
    };
    use pi_agent::{StreamFn, ThinkingLevel};
    use pi_coding::{Application, Session, SessionOptions};

    #[test]
    fn parses_recorded_and_context_excluded_bash() {
        assert_eq!(parse_bash_command("!echo kept"), Some(("echo kept", false)));
        assert_eq!(
            parse_bash_command("!!echo hidden"),
            Some(("echo hidden", true))
        );
        assert_eq!(parse_bash_command("hello"), None);
    }

    /// Session whose provider stream counts invocations and replies with a
    /// fixed text — a faux summarizer for the handoff-prose call.
    fn counting_session(cwd: &Path, calls: Arc<AtomicUsize>, reply: &'static str) -> Session {
        let stream_fn: StreamFn = Arc::new(move |_model, _context, _options| {
            calls.fetch_add(1, Ordering::SeqCst);
            let reply = reply.to_owned();
            Box::pin(async move {
                let stream = new_assistant_message_event_stream();
                let producer = stream.clone();
                tokio::spawn(async move {
                    let mut message = AssistantMessage::pending(&Model::default());
                    message.content.push(ContentBlock::text(reply));
                    message.stop_reason = StopReason::Stop;
                    producer.end(Some(message)).await;
                });
                stream
            })
        });
        Session::new(SessionOptions {
            model: Model::default(),
            cwd: cwd.to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: String::new(),
            compaction: None,
            stream_options: Default::default(),
            tools: Some(Vec::new()),
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: Some(stream_fn),
            auth_resolver: None,
        })
        .expect("session")
    }

    #[tokio::test]
    async fn handoff_prose_renders_envelope_plus_prose_and_bare_stays_envelope_only() {
        let cwd = tempfile::tempdir().expect("cwd");
        let calls = Arc::new(AtomicUsize::new(0));
        let application = Application::new(counting_session(
            cwd.path(),
            calls.clone(),
            "handoff prose reply",
        ))
        .await;

        let prose = run_handoff(&application, "--prose")
            .await
            .expect("prose handoff");
        assert!(
            prose.contains("# Handoff"),
            "--prose must render the envelope:\n{prose}"
        );
        assert!(
            prose.contains("> handoff prose reply"),
            "--prose must render the summarizer prose quoted:\n{prose}"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "--prose runs exactly one summarizer call"
        );

        let envelope = run_handoff(&application, "")
            .await
            .expect("bare handoff");
        assert!(
            envelope.contains("# Handoff"),
            "bare /handoff must render the envelope:\n{envelope}"
        );
        assert!(
            !envelope.contains("> handoff prose reply"),
            "bare /handoff must stay envelope-only:\n{envelope}"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "bare /handoff must not invoke the summarizer"
        );

        application.cleanup().await;
    }

    #[tokio::test]
    async fn handoff_unknown_flag_is_rejected_without_provider_call() {
        let cwd = tempfile::tempdir().expect("cwd");
        let calls = Arc::new(AtomicUsize::new(0));
        let application = Application::new(counting_session(cwd.path(), calls.clone(), "unused"))
            .await;

        let error = run_handoff(&application, "--bogus")
            .await
            .expect_err("unknown /handoff flag must be rejected");
        assert!(
            error.to_string().contains("/handoff [--prose]"),
            "unknown flag must surface usage: {error}"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "a rejected flag must never reach the provider"
        );

        application.cleanup().await;
    }
}
