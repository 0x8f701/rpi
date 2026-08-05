//! Library facade for the `rpi` CLI.
//!
//! Exposes the CLI's modules so integration tests can drive the same code
//! paths as the binary (arg parsing, subcommands, session setup, print mode,
//! and the REPL) without spawning a subprocess.

mod agents_panel;
pub mod approval;
pub mod args;
pub mod auth_commands;
pub mod clipboard;
pub mod code_review;
pub mod code_review_panel;
pub mod side_chat;
pub mod side_chat_panel;

pub mod commands;
pub mod extension_ui;
pub mod file_args;
pub mod file_search;
pub mod human_event_renderer;
pub mod goal_commands;
pub mod image_pipeline;
pub mod job_card_adapter;
pub mod interactive_commands;
pub mod keybindings;
pub mod llama_commands;
pub mod loop_commands;
pub mod markdown;
pub mod models_config;
pub mod orchestration_message;
pub mod modes;
pub mod output;
pub mod package_commands;
pub mod package_config;
pub mod process_commands;
pub mod repl;
pub mod resume_catalog;
mod saved_session_selector;
mod scoped_model_selector;
pub mod self_update;
pub mod session_run;
pub mod session_run_blueprint;
pub mod settings_panel;
pub mod settings_rpc;
pub mod terminal_images;
pub mod tool_card_adapter;
pub mod theme;
pub(crate) mod tree_panel;
pub mod todo_dag_panel;
pub mod tui;
pub mod workflow_commands;
pub mod workflow_rpc;
pub mod workflow_panel;

use anyhow::{Context, Result};

pub use args::{ApprovalModeArg, Cli, Command, LlamaCommand, Mode};

/// Dispatch a parsed [`Cli`]. Subcommands run synchronously; the top-level
/// flags drive print mode or the interactive REPL.
pub async fn run(cli: Cli) -> Result<()> {
    cli.validate().map_err(anyhow::Error::msg)?;
    session_run::set_offline(cli.offline);
    if let Some(search) = cli.list_models.as_deref() {
        return commands::list_models((!search.is_empty()).then_some(search)).await;
    }
    match cli.command {
        Some(Command::Login { provider }) => auth_commands::login_cli(provider.as_deref()).await,
        Some(Command::Logout { provider }) => auth_commands::logout_cli(provider.as_deref()).await,
        Some(Command::Models { filter }) => commands::list_models(filter.as_deref()).await,
        Some(Command::Sessions) => commands::list_sessions(&cli),
        Some(Command::ImportSession {
            ref source,
            ref input,
            ref output,
        }) => commands::import_session_command(&cli, source, input, output.as_deref()),
        Some(Command::Reload) => commands::reload_resources_command(&cli),
        Some(Command::Export {
            session,
            output,
            jsonl,
        }) => commands::export_session_command(&session, output.as_deref(), jsonl),
        Some(Command::Llama { command }) => llama_commands::run(command).await,
        Some(Command::Install { source, local }) => {
            let cwd = cli.cwd.clone().unwrap_or(std::env::current_dir()?);
            package_commands::install_package(&source, local, &cwd)
        }
        Some(Command::Remove { source, local }) => {
            let cwd = cli.cwd.clone().unwrap_or(std::env::current_dir()?);
            package_commands::remove_package(&source, local, &cwd)
        }
        Some(Command::List) => {
            let cwd = cli.cwd.clone().unwrap_or(std::env::current_dir()?);
            package_commands::list_packages(&cwd)
        }
        Some(Command::Config { local }) => {
            let cwd = cli.cwd.clone().unwrap_or(std::env::current_dir()?);
            package_config::config_command(&cwd, local, cli.approve, cli.no_approve).await
        }
        Some(Command::Update {
            self_update,
            extensions,
            all,
            models,
            extension,
            force,
            package,
        }) => {
            let cwd = cli.cwd.clone().unwrap_or(std::env::current_dir()?);
            if models {
                return commands::refresh_model_catalogs().await;
            }
            if let Some(extension) = extension {
                return package_commands::update_package(&extension, &cwd);
            }
            if let Some(package) = package {
                if matches!(package.as_str(), "self" | "rpi") {
                    if extensions {
                        package_commands::update_packages(&cwd)?;
                    }
                    return self_update::update_self(force).await;
                }
                return package_commands::update_package(&package, &cwd);
            }
            if all || (self_update && extensions) {
                package_commands::update_packages(&cwd)?;
                return self_update::update_self(force).await;
            }
            if extensions {
                return package_commands::update_packages(&cwd);
            }
            self_update::update_self(force).await
        }
        None => main_run(&cli).await,
    }
}

async fn main_run(cli: &Cli) -> Result<()> {
    let stdin_tty = std::io::IsTerminal::is_terminal(&std::io::stdin());
    let stdout_tty = std::io::IsTerminal::is_terminal(&std::io::stdout());

    match cli.mode {
        Some(Mode::Json) => run_structured(|| modes::json::run(cli)).await,
        Some(Mode::Rpc) => run_structured(|| modes::rpc::run(cli)).await,
        _ if cli.is_print_mode()
            || (cli.listen.is_none()
                && (!stdin_tty || !stdout_tty)
                && !cli.prompt.is_empty()) =>
        {
            session_run::print_mode(cli).await
        }
        Some(Mode::Text) | None => {
            let session_run::RunSession {
                application,
                extension_ui,
                scoped_models,
                startup_warnings,
                ..
            } = session_run::build_session(cli).await?;
            let initial_prompts = cli.prompt.clone();
            // Interactive terminal users get the TUI; non-TTY contexts (piped
            // stdout, subprocesses, CI) get the line REPL, which runs any
            // initial prompts then exits cleanly on EOF and honors the
            // /help /model ... slash-command contract.
            let listen_handle = match start_listen(cli, &application, &extension_ui).await {
                Ok(handle) => handle,
                Err(error) => {
                    application.cleanup().await;
                    return Err(error.context("starting control plane listener"));
                }
            };
            let result = if stdin_tty && stdout_tty {
                tui::interactive(
                    application.clone(),
                    extension_ui.expect("TUI composition always provides an extension UI adapter"),
                    scoped_models,
                    initial_prompts,
                    startup_warnings,
                )
                .await
            } else {
                // REPL: settings warnings were already emitted to stderr during
                // build_session (capture was not armed for non-TUI modes).
                repl::interactive(application.clone(), initial_prompts).await
            };
            let stop_result = match listen_handle {
                Some(handle) => handle.stop().await.context("stopping control plane listener"),
                None => Ok(()),
            };
            application.cleanup().await;
            combine_run_and_stop_results(result, stop_result)
        }
    }
}

fn combine_run_and_stop_results(run_result: Result<()>, stop_result: Result<()>) -> Result<()> {
    match (run_result, stop_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(run_error), Ok(())) => Err(run_error),
        (Ok(()), Err(stop_error)) => Err(stop_error),
        (Err(run_error), Err(stop_error)) => Err(run_error.context(format!(
            "control plane listener also failed during shutdown: {stop_error:#}"
        ))),
    }
}

/// Start the opt-in `--listen` control plane when requested.
///
/// The listener shares the same live [`Application`] used by the TUI/REPL and
/// is stopped after the UI exits, before `application.cleanup`. Bind, auth,
/// and read failures are startup errors.
async fn start_listen(
    cli: &Cli,
    application: &pi_coding::Application,
    extension_ui: &Option<crate::extension_ui::ExtensionUiAdapter>,
) -> Result<Option<modes::listen::ListenHandle>> {
    let Some(address) = cli.listen else {
        return Ok(None);
    };
    let extension_ui = extension_ui.clone().unwrap_or_default();
    let config = modes::listen::ListenConfig {
        address,
        token_file: cli.listen_token_file.clone(),
    };
    let handle = modes::listen::start(application.clone(), extension_ui, config).await?;
    let auth_enabled = cli.listen_token_file.is_some();
    eprintln!(
        "Control plane listening on http://{} ({})",
        handle.local_addr(),
        if auth_enabled {
            "authentication enabled"
        } else {
            "loopback only"
        }
    );
    Ok(Some(handle))
}

async fn run_structured<F, Fut>(run: F) -> Result<()>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<()>>,
{
    match run().await {
        Ok(()) => Ok(()),
        Err(error) => modes::json::write_json_line(
            &mut std::io::stdout().lock(),
            &modes::rpc::RpcResponse::failure(None, "initialize", error.to_string()),
        ),
    }
}
