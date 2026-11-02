//! Library facade for the `rpi` CLI.
//!
//! Exposes the CLI's modules so integration tests can drive the same code
//! paths as the binary (arg parsing, subcommands, session setup, print mode,
//! and the REPL) without spawning a subprocess.

mod agents_panel;
mod ansi;
pub mod approval;
pub mod args;
pub mod auth_commands;
mod auth_json;
pub mod clipboard;
pub mod code_review;
pub mod code_review_panel;
pub mod chat_sync;
pub mod collab_commands;
pub mod collab_guest;
pub mod side_chat;
pub mod side_chat_panel;

pub mod commands;
pub mod dependency_map;
pub mod doctor;
pub mod extension_ui;
pub mod file_args;
pub(crate) mod file_conversation;
pub mod file_search;
pub(crate) mod file_explorer;
pub(crate) mod file_viewer;
pub mod goal_commands;
pub mod human_event_renderer;
pub mod image_pipeline;
pub mod interactive_commands;
pub mod job_card_adapter;
pub(crate) mod jobs_panel;
pub mod keybindings;
pub mod llama_commands;
pub mod loop_commands;
pub mod markdown;
pub mod mcp_commands;
pub mod models_config;
pub mod modes;
pub mod orchestration_message;
pub mod output;
pub mod package_commands;
pub mod package_config;
pub mod plugin_commands;
pub mod process_commands;
pub mod repl;
pub mod resume_catalog;
pub mod runner_identity;
mod saved_session_selector;
mod scoped_model_selector;
pub mod self_update;
pub mod session_run;
pub mod session_run_blueprint;
pub mod settings_config;
pub mod settings_panel;
pub mod settings_rpc;
pub mod team;
mod team_commands;
pub mod terminal_images;
pub mod theme;
mod todo_child_commands;
mod todo_child_detail;
pub mod todo_dag_panel;
mod todo_dag_view;
pub mod tool_card_adapter;
pub(crate) mod tree_panel;
pub mod tui;
#[doc(hidden)]
pub mod tui_test_support;
pub(crate) mod video_extract;
pub(crate) mod pdf_extract;
pub(crate) mod tmux_file;

pub(crate) mod web;
pub mod workflow_commands;
pub mod workflow_panel;
pub mod workflow_rpc;

#[cfg(test)]
pub(crate) mod test_support;

use anyhow::{Context, Result, anyhow};

pub use args::{
    AgentCommand, ApprovalModeArg, Cli, Command, CompletionShell, ConfigCommand, ConfigScopeArg,
    LlamaCommand, McpCommand, McpImportSourceArg, Mode, PluginCommand,
};

/// Best-effort hardening for the normal CLI process, run once at startup so a
/// crash cannot leak in-memory secrets (sessions, API keys) through core dumps
/// or same-user inspection.
///
/// - Linux: makes the process non-dumpable (`PR_SET_DUMPABLE=0`), denying
///   ptrace attach and `/proc/<pid>/mem` access, and sets `RLIMIT_CORE=0` so
///   crashes cannot write core dumps.
/// - Other unix: `RLIMIT_CORE=0` where supported.
/// - Non-unix: no-op.
///
/// Every syscall is cfg-guarded and failure-ignored: this is best-effort
/// hardening and must never break startup on unsupported platforms. Loader
/// variables (`LD_PRELOAD`, `LD_LIBRARY_PATH`) are consumed by the dynamic
/// loader before `main` runs and cannot be sanitized after the fact; child
/// processes already rebuild their environments (pi-coding tools and
/// extensions).
pub fn harden_process() {
    #[cfg(unix)]
    pi_sandbox::harden_current_process();
}
#[cfg(unix)]
pub fn sandbox_child_main() -> Result<std::convert::Infallible, pi_sandbox::SandboxError> {
    pi_sandbox::exec_child_target(3, 4)
}

/// Detect the exact `__sandbox-child` sentinel argument slice used by the
/// binary entry point to short-circuit into [`sandbox_child_main`] before
/// normal CLI startup.
#[cfg(unix)]
pub fn is_sandbox_child_dispatch(args: &[std::ffi::OsString]) -> bool {
    args == [
        std::ffi::OsString::from("__sandbox-child"),
        std::ffi::OsString::from("3"),
        std::ffi::OsString::from("4"),
    ]
}

/// Dispatch a parsed [`Cli`]. Subcommands run synchronously; top-level flags
/// select structured, print, Web-listener, TUI, or line-REPL execution.
pub async fn run(mut cli: Cli) -> Result<()> {
    cli.validate().map_err(anyhow::Error::msg)?;
    session_run::set_offline(cli.offline);
    // Resolve and install the active config profile (CLI `--profile` wins
    // over `PI_PROFILE`) before any dispatch resolves an agent-dir-derived
    // path, so settings, auth, sessions, memory, and skills all relocate
    // under `<base>/profiles/<name>`.
    session_run::activate_profile(&cli)?;
    if let Some(search) = cli.list_models.as_deref() {
        let application = session_run::build_operator_application(&cli).await?;
        let result =
            commands::list_models((!search.is_empty()).then_some(search), &application).await;
        application.cleanup().await;
        return result;
    }
    // Top-level `--export SESSION_PATH` mirrors the `export` subcommand (same
    // session -> HTML/JSONL path, honoring `-o/--output` and `--jsonl`).
    if let Some(session) = cli.export.as_deref() {
        return commands::export_session_command(session, cli.output.as_deref(), cli.jsonl);
    }
    // Detach the subcommand before dispatch so payloads can move into their
    // handlers while the remaining CLI options stay borrowable for the one
    // normal Application/session build.
    let command = cli.command.take();
    match command {
        // `rpi rpc` ≡ `rpi --mode rpc` (the successor of the removed `rpi-rpc`
        // companion binary): force RPC headless mode and dispatch through the
        // top-level mode path. A conflicting explicit `--mode` was already
        // rejected by `validate`; the detached command keeps `main_run` from
        // re-entering this match.
        Some(Command::Rpc) => {
            cli.mode = Some(Mode::Rpc);
            main_run(&cli).await
        }
        Some(Command::Login { provider, scope, json }) => {
            if json {
                auth_json::login(provider.as_deref(), scope.as_deref()).await
            } else {
                auth_commands::login_cli(provider.as_deref(), scope.as_deref()).await
            }
        }
        Some(Command::Logout { provider, scope }) => {
            auth_commands::logout_cli(provider.as_deref(), scope.as_deref()).await
        }
        Some(Command::Models { filter }) => {
            let application = session_run::build_operator_application(&cli).await?;
            let result = commands::list_models(filter.as_deref(), &application).await;
            application.cleanup().await;
            result
        }
        Some(Command::Sessions) => commands::list_sessions(&cli),
        Some(Command::ImportSession {
            ref source,
            ref input,
            ref output,
        }) => commands::import_session_command(&cli, source, input, output.as_deref()),
        Some(Command::Reload) => commands::reload_resources_command(&cli),
        Some(Command::Doctor { json }) => doctor::doctor_command(&cli, json),
        Some(Command::Setup { json }) => doctor::setup_command(json),
        Some(Command::Dashboard { json }) => doctor::dashboard_command(&cli, json),
        Some(Command::Export {
            session,
            output,
            jsonl,
        }) => commands::export_session_command(&session, output.as_deref(), jsonl),
        Some(Command::Llama { command }) => {
            let application = session_run::build_operator_application(&cli).await?;
            let result = llama_commands::run(command, &application).await;
            application.cleanup().await;
            result
        }
        Some(Command::Plugin { command }) => {
            let cwd = cli.cwd.clone().unwrap_or(std::env::current_dir()?);
            let application = session_run::build_operator_application(&cli).await?;
            let process = application.governed_process_context();
            let http = pi_coding::plugin::PluginHttpExecutors::new(
                application.http_executor(pi_coding::HostHttpClass::PluginMarketplace),
                application.http_executor(pi_coding::HostHttpClass::PluginArchive),
                application.http_executor(pi_coding::HostHttpClass::NpmRegistry),
                application.http_executor(pi_coding::HostHttpClass::NpmTarball),
            );
            let result = plugin_commands::run(command, &cwd, process, http).await;
            application.cleanup().await;
            result
        }
        Some(Command::Agent { ref command }) => match command {
            crate::args::AgentCommand::Stdio => modes::acp::run_stdio(cli.clone()).await,
            crate::args::AgentCommand::Serve {
                address,
                token_file,
            } => modes::acp::run_serve(cli.clone(), *address, token_file.clone()).await,
        },
        Some(Command::Mcp { command }) => {
            let cwd = cli.cwd.clone().unwrap_or(std::env::current_dir()?);
            mcp_commands::run(command, &cwd)
        }
        Some(Command::Install { source, local }) => {
            let cwd = cli.cwd.clone().unwrap_or(std::env::current_dir()?);
            let application = session_run::build_operator_application(&cli).await?;
            let process = application.governed_process_context();
            let result = package_commands::install_package(&source, local, &cwd, process);
            application.cleanup().await;
            result
        }
        Some(Command::Remove { source, local }) => {
            let cwd = cli.cwd.clone().unwrap_or(std::env::current_dir()?);
            package_commands::remove_package(&source, local, &cwd)
        }
        Some(Command::List) => {
            let cwd = cli.cwd.clone().unwrap_or(std::env::current_dir()?);
            let application = session_run::build_operator_application(&cli).await?;
            let process = application.governed_process_context();
            let result = package_commands::list_packages(&cwd, process);
            application.cleanup().await;
            result
        }
        Some(Command::Config { local, command }) => {
            let cwd = cli.cwd.clone().unwrap_or(std::env::current_dir()?);
            if let Some(command) = command {
                settings_config::run(command, &cwd, local, cli.approve, cli.no_approve)
            } else {
                package_config::config_command(&cwd, local, cli.approve, cli.no_approve).await
            }
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
                let application = session_run::build_operator_application(&cli).await?;
                let result = commands::refresh_model_catalogs(&application).await;
                application.cleanup().await;
                return result;
            }
            let package_operation = extension.as_deref().or_else(|| {
                package
                    .as_deref()
                    .filter(|package| !matches!(*package, "self" | "rpi"))
            });
            if package_operation.is_some() || extensions || all {
                let application = session_run::build_operator_application(&cli).await?;
                let process = application.governed_process_context();
                let package_result = if let Some(source) = package_operation {
                    package_commands::update_package(source, &cwd, process)
                } else {
                    package_commands::update_packages(&cwd, process)
                };
                application.cleanup().await;
                package_result?;
                if package_operation.is_some() || (extensions && !self_update && !all) {
                    return Ok(());
                }
            }
            self_update::update_self(force).await
        }
        Some(Command::Completion { shell }) => {
            args::write_completion(shell, &mut std::io::stdout());
            Ok(())
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
            || (cli.listen.is_none() && (!stdin_tty || !stdout_tty) && !cli.prompt.is_empty()) =>
        {
            session_run::print_mode(cli).await
        }
        Some(Mode::Text) | None if cli.listen.is_some() => run_listener_mode(cli).await,
        Some(Mode::Text) | None => {
            let session_run::RunSession {
                application,
                extension_ui,
                scoped_models,
                startup_warnings,
                resume_notice,
                todo_child_service,
                ..
            } = session_run::build_session(cli).await?;
            let initial_prompts = cli.prompt.clone();
            let result = if stdin_tty && stdout_tty {
                tui::interactive(
                    application.clone(),
                    extension_ui.expect("TUI composition always provides an extension UI adapter"),
                    scoped_models,
                    initial_prompts,
                    startup_warnings,
                    resume_notice,
                    None,
                )
                .await
            } else {
                repl::interactive(application.clone(), initial_prompts, None)
                    .await
                    .map(|()| tui::TuiExit::Terminal)
            };
            if result
                .as_ref()
                .is_ok_and(|exit| *exit == tui::TuiExit::Restart)
            {
                if let Err(error) = pi_coding::Application::checkpoint_process_for_restart(
                    std::slice::from_ref(&application),
                    Some(todo_child_service.as_ref()),
                ) {
                    let shutdown = todo_child_service.shutdown().await.context(
                        "shutting down Todo child service after TUI restart checkpoint failure",
                    );
                    let cleanup = application.cleanup().await;
                    return combine_tui_shutdown(
                        Err(error.context("checkpointing TUI process restart")),
                        shutdown,
                        cleanup,
                    );
                }
                return Ok(());
            }
            let shutdown = todo_child_service
                .shutdown()
                .await
                .context("shutting down Todo child service");
            combine_tui_shutdown(result.map(|_| ()), shutdown, application.cleanup().await)
        }
    }
}
fn combine_tui_shutdown(
    run_result: Result<()>,
    todo_shutdown: Result<()>,
    cleanup: Result<()>,
) -> Result<()> {
    let mut errors = Vec::new();
    if let Err(error) = run_result {
        errors.push(format!("{error:#}"));
    }
    if let Err(error) = todo_shutdown {
        errors.push(format!("shutting down Todo child service: {error:#}"));
    }
    if let Err(error) = cleanup {
        errors.push(format!("cleaning up primary application: {error:#}"));
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(anyhow!(errors.join("; ")))
    }
}

/// Run `--listen` as a Web-only backend. Standard input is deliberately never
/// read: a closed pipe must not stop the service, and a terminal must never be
/// acquired. The listener owns the live application until a process signal.
/// Process host shutdown stops the listener, shuts the host-owned Todo child
/// service, then runs parent `application.cleanup`. Per-session close and
/// WebSocket disconnect never reach this path.
async fn run_listener_mode(cli: &Cli) -> Result<()> {
    let mut shutdown = ListenerShutdownSignals::new()?;
    let session_run::RunSession {
        application,
        extension_ui,
        spawner,
        todo_child_service,
        ..
    } = session_run::build_session(cli).await?;
    let handle = match start_listen(cli, &application, &extension_ui, spawner).await {
        Ok(Some(handle)) => handle,
        Ok(None) => unreachable!("listener mode requires --listen"),
        Err(error) => {
            let error = error.context("starting control plane listener");
            let shutdown = todo_child_service
                .shutdown()
                .await
                .context("shutting down Todo child service");
            application.cleanup().await;
            return match shutdown {
                Ok(()) => Err(error),
                Err(shutdown_error) => Err(error.context(format!(
                    "Todo child service shutdown also failed: {shutdown_error:#}"
                ))),
            };
        }
    };
    let is_restart = shutdown.wait().await == ListenerShutdownIntent::Restart;
    let stop_result = handle.stop(is_restart).await;
    if is_restart {
        // A successful restart checkpoint detaches alone: the checkpointed
        // process state survives for the successor, so the primary
        // application and the host-owned Todo child service are never torn
        // down. A failed checkpoint is terminal: the manager has already
        // terminally cleaned its owned secondaries and published the exact
        // checkpoint Err — this side runs the same primary/Todo cleanup the
        // terminal path runs and returns that error RAW. It is never
        // re-contextualized (no "stopping control plane listener" layer on
        // top): the terminal cause is the checkpoint failure itself. Cleanup
        // failures are diagnostics on stderr so the exact Err stays exact.
        return match stop_result {
            Ok(()) => Ok(()),
            Err(error) => {
                if let Err(shutdown) = todo_child_service.shutdown().await {
                    eprintln!(
                        "listener_shutdown category=todo_shutdown_after_restart_checkpoint_failure error={shutdown:#}"
                    );
                }
                if let Err(cleanup) = application.cleanup().await {
                    eprintln!(
                        "listener_shutdown category=primary_cleanup_after_restart_checkpoint_failure error={cleanup}"
                    );
                }
                Err(error)
            }
        };
    }
    let stop_result = stop_result.context("stopping control plane listener");
    let shutdown = todo_child_service
        .shutdown()
        .await
        .context("shutting down Todo child service");
    let cleanup = application.cleanup().await;
    combine_listener_shutdown(stop_result, shutdown, cleanup)
}

fn combine_listener_shutdown(
    stop_result: Result<()>,
    todo_shutdown: Result<()>,
    cleanup: Result<()>,
) -> Result<()> {
    let mut errors = Vec::new();
    if let Err(error) = stop_result {
        errors.push(format!("stopping control plane listener: {error:#}"));
    }
    if let Err(error) = todo_shutdown {
        errors.push(format!("shutting down Todo child service: {error:#}"));
    }
    if let Err(error) = cleanup {
        errors.push(format!("cleaning up primary application: {error:#}"));
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(anyhow!(errors.join("; ")))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ListenerShutdownIntent {
    Terminal,
    Restart,
}

#[cfg(unix)]
fn listener_shutdown_intent_for_signal(
    signal: tokio::signal::unix::SignalKind,
) -> ListenerShutdownIntent {
    use tokio::signal::unix::SignalKind;

    if signal == SignalKind::interrupt() {
        ListenerShutdownIntent::Terminal
    } else {
        ListenerShutdownIntent::Restart
    }
}

#[cfg(unix)]
struct ListenerShutdownSignals {
    interrupt: tokio::signal::unix::Signal,
    terminate: tokio::signal::unix::Signal,
    hangup: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl ListenerShutdownSignals {
    fn new() -> Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};

        Ok(Self {
            interrupt: signal(SignalKind::interrupt()).context("installing SIGINT handler")?,
            terminate: signal(SignalKind::terminate()).context("installing SIGTERM handler")?,
            hangup: signal(SignalKind::hangup()).context("installing SIGHUP handler")?,
        })
    }

    async fn wait(&mut self) -> ListenerShutdownIntent {
        tokio::select! {
            _ = self.interrupt.recv() => listener_shutdown_intent_for_signal(tokio::signal::unix::SignalKind::interrupt()),
            _ = self.terminate.recv() => listener_shutdown_intent_for_signal(tokio::signal::unix::SignalKind::terminate()),
            _ = self.hangup.recv() => listener_shutdown_intent_for_signal(tokio::signal::unix::SignalKind::hangup()),
        }
    }
}

#[cfg(not(unix))]
struct ListenerShutdownSignals;

#[cfg(not(unix))]
impl ListenerShutdownSignals {
    fn new() -> Result<Self> {
        Ok(Self)
    }

    async fn wait(&mut self) -> ListenerShutdownIntent {
        let _ = tokio::signal::ctrl_c().await;
        ListenerShutdownIntent::Terminal
    }
}

fn pairing_qr(url: &str) -> String {
    qrcode::QrCode::new(url.as_bytes())
        .map(|code| {
            code.render::<qrcode::render::unicode::Dense1x2>()
                .quiet_zone(true)
                .build()
        })
        .map(|rendered| format!("\n{rendered}"))
        .unwrap_or_default()
}

/// Start the opt-in `--listen` control plane.
///
/// The listener shares the Web-only mode's live [`Application`] and remains
/// active until a process signal, then stops. Host shutdown then shuts the
/// shared Todo child service before `application.cleanup`.
/// [`RunSessionSpawner`] builds manager-owned session runtimes for the Web
/// control plane from the same host service Arc. Bind, auth, and read
/// failures are startup errors.
async fn start_listen(
    cli: &Cli,
    application: &pi_coding::Application,
    extension_ui: &Option<crate::extension_ui::ExtensionUiAdapter>,
    spawner: session_run::RunSessionSpawner,
) -> Result<Option<modes::listen::ListenHandle>> {
    let Some(address) = cli.listen else {
        return Ok(None);
    };
    let extension_ui = extension_ui.clone().unwrap_or_default();
    let config = modes::listen::ListenConfig {
        address,
        token_file: cli.listen_token_file.clone(),
        allow_insecure_remote: cli.listen_allow_insecure_remote,
        advertised_origin: cli.listen_advertised_origin.clone(),
        runner_codename_override: cli.listen_codename.clone(),
        plaintext: cli.listen_plaintext,
        tls_cert: cli.listen_cert.clone(),
        tls_key: cli.listen_key.clone(),
        session_factory: Some(std::sync::Arc::new(spawner)),
    };
    let handle = modes::listen::start(application.clone(), extension_ui, config).await?;
    let addr = handle.local_addr();
    // Directly openable Web UI URL: an explicit advertised origin, a
    // concrete bind, or — for a wildcard bind — a best-effort discovered
    // LAN IP. When none is reachable the banner still prints a useful
    // textual fallback rather than an unreachable 0.0.0.0/:: URL. The
    // collaboration base_url stays fail-closed for wildcard binds; this
    // display path is human-facing only.
    let port = addr.port();
    let web_line = match handle.display_web_url() {
        Some(base) => {
            let token = cli.listen_token_file.as_deref()
                .map(std::fs::read_to_string)
                .transpose()
                .ok()
                .flatten()
                .map(|t| t.trim().to_owned())
                .unwrap_or_default();
            if token.is_empty() {
                format!(" Web UI: {base}/web")
            } else {
                let pair_url = format!("{base}/chat#{token}");
                let qr_line = pairing_qr(&pair_url);
                format!(" Web UI: {base}/web#{token}\n        (scan or open this URL — the token pairs automatically){qr_line}")
            }
        }
        None => format!(" Web UI: use this machine's LAN IP on port {port}"),
    };
    let auth = if cli.listen_token_file.is_some() {
        "authentication enabled"
    } else {
        "tokenless"
    };
    let scheme = if cli.listen_plaintext {
        "http"
    } else {
        "https"
    };
    eprintln!("Control plane listening on {scheme}://{addr} ({auth}).{web_line}");
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

/// Whether the interactive TUI should force full-color output.
///
/// rpi follows omp and overrides `NO_COLOR` for its interactive TUI: crossterm
/// gates every SGR color sequence on `NO_COLOR` (memoized on first use), so an
/// environment that exports it rendered the TUI near-monochrome — only
/// `SetAttribute`-driven bold survived. The TUI forces truecolor output
/// whenever the terminal is genuinely color-capable; only a color-less
/// terminal (`TERM=dumb`) or a non-TTY stream stays monochrome.
///
/// `NO_COLOR` is deliberately not consulted here: like omp's `FORCE_COLOR=1`,
/// the interactive UI always renders its full theme when the terminal can show
/// it. Print mode never calls [`force_tui_color`] and keeps its existing
/// SGR-free-when-piped output.
pub(crate) fn tui_color_forced(stdout_is_terminal: bool, term: Option<&str>) -> bool {
    stdout_is_terminal && term != Some("dumb")
}

/// Open crossterm's `NO_COLOR` gate for the interactive TUI's rendering
/// backend, when the terminal supports color.
///
/// Ratatui renders through the crossterm version it depends on, which memoizes
/// `NO_COLOR` and then suppresses every color command (`SetColors` writes an
/// empty sequence), leaving only bold. Reopening that gate via crossterm's own
/// `force_color_output` guarantees the full truecolor theme. Must run before
/// the first frame is drawn.
pub(crate) fn force_tui_color() {
    if tui_color_forced(
        std::io::IsTerminal::is_terminal(&std::io::stdout()),
        std::env::var("TERM").ok().as_deref(),
    ) {
        ratatui::crossterm::style::force_color_output(true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tui_color_forced_ignores_no_color_for_capable_terminals() {
        // NO_COLOR=1 is deliberately overridden (omp behavior): a
        // color-capable terminal always gets the full theme...
        assert!(tui_color_forced(true, Some("xterm-256color")));
        assert!(tui_color_forced(true, Some("tmux-256color")));
        assert!(tui_color_forced(true, None)); // TERM unset: assume color
        // ...while a terminal that truly cannot do color, or a non-TTY stream,
        // stays monochrome.
        assert!(!tui_color_forced(true, Some("dumb")));
        assert!(!tui_color_forced(false, Some("xterm-256color")));
        assert!(!tui_color_forced(false, None));
    }

    #[test]
    fn force_tui_color_reopens_the_crossterm_gate_ratatui_uses() {
        use ratatui::crossterm::style::{Color, Colored};

        // Simulate the NO_COLOR=1 world: the gate crossterm memoized is closed,
        // so ratatui's SetColors writes an empty SGR sequence — the "only
        // bold" rendering from the T31 1v1 comparison.
        let previous = Colored::ansi_color_disabled_memoized();
        Colored::set_ansi_color_disabled(true);
        assert!(Colored::ansi_color_disabled_memoized());

        // The TUI force reopens exactly that gate.
        ratatui::crossterm::style::force_color_output(true);
        assert!(!Colored::ansi_color_disabled_memoized());
        assert_eq!(Colored::ForegroundColor(Color::Red).to_string(), "38;5;9");

        Colored::set_ansi_color_disabled(previous);
    }

    #[cfg(unix)]
    #[test]
    fn listener_signal_intents_match_restart_contract() {
        use tokio::signal::unix::SignalKind;

        assert_eq!(
            listener_shutdown_intent_for_signal(SignalKind::interrupt()),
            ListenerShutdownIntent::Terminal,
        );
        assert_eq!(
            listener_shutdown_intent_for_signal(SignalKind::terminate()),
            ListenerShutdownIntent::Restart,
        );
        assert_eq!(
            listener_shutdown_intent_for_signal(SignalKind::hangup()),
            ListenerShutdownIntent::Restart,
        );
    }

    #[test]
    fn tui_shutdown_combines_context_from_every_failure() {
        let error = combine_tui_shutdown(
            Err(anyhow!("run root").context("run context")),
            Err(anyhow!("todo root").context("todo context")),
            Err(anyhow!("cleanup root").context("cleanup context")),
        )
        .expect_err("combined shutdown must retain every failure");
        let message = error.to_string();
        assert!(message.contains("run context: run root"), "{message}");
        assert!(message.contains("todo context: todo root"), "{message}");
        assert!(
            message.contains("cleanup context: cleanup root"),
            "{message}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn sandbox_child_dispatch_sentinel_is_exact() {
        use std::ffi::OsString;
        // Only the exact 3-argument sentinel dispatches; any deviation
        // falls through to normal CLI parsing.
        assert!(is_sandbox_child_dispatch(&[
            OsString::from("__sandbox-child"),
            OsString::from("3"),
            OsString::from("4"),
        ]));
        assert!(!is_sandbox_child_dispatch(&[]));
        assert!(!is_sandbox_child_dispatch(&[OsString::from(
            "__sandbox-child"
        ),]));
        assert!(!is_sandbox_child_dispatch(&[
            OsString::from("__sandbox-child"),
            OsString::from("3"),
        ]));
        assert!(!is_sandbox_child_dispatch(&[
            OsString::from("__sandbox-child"),
            OsString::from("3"),
            OsString::from("4"),
            OsString::from("extra"),
        ]));
        assert!(!is_sandbox_child_dispatch(&[OsString::from("--help"),]));
    }

    #[cfg(unix)]
    #[test]
    fn harden_process_runs_without_panic() {
        // Best-effort hardening is invoked at startup on every unix platform
        // and must never panic or otherwise break the entry point, even when
        // the underlying syscalls fail.
        harden_process();
    }

    /// CLI-surface smoke: the top-level `--export` flag must drive the same
    /// session -> HTML export as the `export` subcommand (content correctness
    /// is covered by the pi-coding export unit tests).
    #[tokio::test]
    async fn top_level_export_flag_writes_non_empty_html() {
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("session.jsonl");
        let out = dir.path().join("export.html");
        std::fs::write(
            &session,
            concat!(
                "{\"type\":\"session\",\"version\":3,\"id\":\"s1\",\"timestamp\":\"2024-01-01T00:00:00Z\",\"cwd\":\"/tmp\"}\n",
                "{\"type\":\"message\",\"id\":\"a\",\"parentId\":null,\"timestamp\":\"2024-01-01T00:00:01Z\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"hello & <world>\"}],\"timestamp\":0}}\n",
            ),
        )
        .unwrap();
        let cli = Cli::try_parse_from([
            "rpi",
            "--export",
            session.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
        ])
        .expect("parse top-level --export");
        run(cli).await.expect("top-level --export dispatch");
        let html = std::fs::read_to_string(&out).expect("exported html written");
        assert!(
            html.contains("<!DOCTYPE html>"),
            "exported page must be self-contained html"
        );
        assert!(html.len() > 1024, "exported html unexpectedly small");
    }

    #[test]
    fn pairing_qr_renders_unicode_blocks_for_dummy_url() {
        let url = "https://192.0.2.10:8765/chat#example-token";
        let qr = pairing_qr(url);
        assert!(!qr.is_empty(), "{url}");
        assert!(
            qr.contains('\u{2580}') || qr.contains('\u{2584}') || qr.contains('\u{2588}'),
            "{url}"
        );
    }

    #[test]
    fn pairing_qr_empty_or_overlong_does_not_panic() {
        let empty = pairing_qr("");
        let _ = empty.len();
        let overlong = pairing_qr(&"x".repeat(8_192));
        let _ = overlong.len();
    }

    #[test]
    fn listen_banner_keeps_scan_hint_next_to_pairing_qr() {
        let src = include_str!("lib.rs");
        let qr_at = src
            .find("pairing_qr(&pair_url)")
            .expect("listen banner must call pairing_qr");
        let hint_at = src
            .find("(scan or open this URL")
            .expect("listen banner must keep scan hint");
        assert!(
            qr_at.abs_diff(hint_at) < 200,
            "scan hint must sit next to pairing_qr call"
        );
    }
}
