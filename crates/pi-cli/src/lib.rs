//! Library facade for the `rpi` CLI.
//!
//! Exposes the CLI's modules so integration tests can drive the same code
//! paths as the binary (arg parsing, subcommands, session setup, print mode,
//! and the REPL) without spawning a subprocess.

mod agents_panel;
pub mod args;
pub mod auth_commands;
pub mod clipboard;
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
mod session_run_blueprint;
pub mod settings_panel;
pub mod settings_rpc;
pub mod terminal_images;
pub mod tool_card_adapter;
pub mod theme;
pub(crate) mod tree_panel;
pub mod tui;
pub mod workflow_commands;
pub mod workflow_rpc;
pub mod workflow_panel;

use anyhow::Result;

pub use args::{Cli, Command, LlamaCommand, Mode};

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
        Some(Command::Sessions) => {
            let cwd = match &cli.cwd {
                Some(dir) => dir.clone(),
                None => std::env::current_dir()?,
            };
            commands::list_sessions(&cwd)
        }
        Some(Command::ImportSession {
            source,
            input,
            output,
        }) => commands::import_session_command(&source, &input, output.as_deref()),
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
        _ if cli.is_print_mode() || ((!stdin_tty || !stdout_tty) && !cli.prompt.is_empty()) => {
            session_run::print_mode(cli).await
        }
        Some(Mode::Text) | None => {
            let session_run::RunSession {
                application,
                extension_ui,
                scoped_models,
                ..
            } = session_run::build_session(cli).await?;
            let initial_prompts = cli.prompt.clone();
            // Interactive terminal users get the TUI; non-TTY contexts (piped
            // stdout, subprocesses, CI) get the line REPL, which runs any
            // initial prompts then exits cleanly on EOF and honors the
            // /help /model ... slash-command contract.
            let result = if stdin_tty && stdout_tty {
                tui::interactive(
                    application.clone(),
                    extension_ui.expect("TUI composition always provides an extension UI adapter"),
                    scoped_models,
                    initial_prompts,
                )
                .await
            } else {
                repl::interactive(application.clone(), initial_prompts).await
            };
            application.cleanup().await;
            result
        }
    }
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
