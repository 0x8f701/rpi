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
pub mod collab_guest;
pub mod collab_commands;

pub mod commands;
pub mod doctor;
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
pub mod mcp_commands;
pub mod models_config;
pub mod orchestration_message;
pub mod modes;
pub mod output;
pub mod package_commands;
pub mod package_config;
pub mod plugin_commands;
pub mod process_commands;
pub mod repl;
pub mod resume_catalog;
mod saved_session_selector;
mod scoped_model_selector;
pub mod self_update;
pub mod session_run;
pub mod session_run_blueprint;
pub mod settings_config;
pub mod settings_panel;
pub mod settings_rpc;
pub mod terminal_images;
pub mod tool_card_adapter;
pub mod theme;
pub(crate) mod tree_panel;
pub mod todo_dag_panel;
mod todo_dag_view;
pub mod tui;
pub(crate) mod web;
pub mod workflow_commands;
pub mod workflow_rpc;
pub mod workflow_panel;

use anyhow::{Context, Result};

pub use args::{AgentCommand, ApprovalModeArg, Cli, Command, CompletionShell, ConfigCommand, ConfigScopeArg, LlamaCommand, McpCommand, McpImportSourceArg, Mode, PluginCommand};

/// Best-effort parent-process hardening, run once at CLI startup before any
/// dispatch so a crash cannot leak in-memory secrets (sessions, API keys)
/// through core dumps or same-user inspection.
///
/// - Linux: makes the process non-dumpable (`PR_SET_DUMPABLE=0`), denying
///   ptrace attach and `/proc/<pid>/mem` access, and sets `RLIMIT_CORE=0` so
///   crashes cannot write core dumps.
/// - Other unix: `RLIMIT_CORE=0` where supported.
/// - Non-unix: no-op.
///
/// Every call is cfg-guarded and failure-ignored: this is best-effort
/// hardening and must never break startup on unsupported platforms. Loader
/// variables (`LD_PRELOAD`, `LD_LIBRARY_PATH`) are consumed by the dynamic
/// loader before `main` runs and cannot be sanitized after the fact; child
/// processes already rebuild their environments (pi-coding tools and
/// extensions).
pub fn harden_process() {
    // Deny ptrace attach and /proc/<pid>/mem access even to same-user
    // debuggers (Linux-only; nix gates the safe prctl wrapper on linux).
    #[cfg(target_os = "linux")]
    {
        use nix::sys::prctl::set_dumpable;
        let _ = set_dumpable(false);
    }
    // Disable core dumps: RLIMIT_CORE = 0 (unix, including Linux).
    #[cfg(unix)]
    {
        use nix::sys::resource::{Resource, setrlimit};
        let _ = setrlimit(Resource::RLIMIT_CORE, 0, 0);
    }
}

/// Dispatch a parsed [`Cli`]. Subcommands run synchronously; top-level flags
/// select structured, print, Web-listener, TUI, or line-REPL execution.
pub async fn run(cli: Cli) -> Result<()> {
    cli.validate().map_err(anyhow::Error::msg)?;
    session_run::set_offline(cli.offline);
    // Resolve and install the active config profile (CLI `--profile` wins
    // over `PI_PROFILE`) before any dispatch resolves an agent-dir-derived
    // path, so settings, auth, sessions, memory, and skills all relocate
    // under `<base>/profiles/<name>`.
    session_run::activate_profile(&cli)?;
    if let Some(search) = cli.list_models.as_deref() {
        return commands::list_models((!search.is_empty()).then_some(search)).await;
    }
    // Top-level `--export SESSION_PATH` mirrors the `export` subcommand (same
    // session -> HTML/JSONL path, honoring `-o/--output` and `--jsonl`).
    if let Some(session) = cli.export.as_deref() {
        return commands::export_session_command(session, cli.output.as_deref(), cli.jsonl);
    }
    match cli.command {
        // `rpi rpc` ≡ `rpi --mode rpc` (the successor of the removed `rpi-rpc`
        // companion binary): force RPC headless mode and dispatch through the
        // top-level mode path. A conflicting explicit `--mode` was already
        // rejected by `validate`. The clone nulls the subcommand so `main_run`
        // dispatches on the forced mode instead of re-entering this match.
        Some(Command::Rpc) => {
            let mut rpc_cli = cli.clone();
            rpc_cli.command = None;
            rpc_cli.mode = Some(Mode::Rpc);
            main_run(&rpc_cli).await
        }
        Some(Command::Login { provider, scope }) => {
            auth_commands::login_cli(provider.as_deref(), scope.as_deref()).await
        }
        Some(Command::Logout { provider, scope }) => {
            auth_commands::logout_cli(provider.as_deref(), scope.as_deref()).await
        }
        Some(Command::Models { filter }) => commands::list_models(filter.as_deref()).await,
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
        Some(Command::Llama { command }) => llama_commands::run(command).await,
        Some(Command::Plugin { command }) => {
            let cwd = cli.cwd.clone().unwrap_or(std::env::current_dir()?);
            plugin_commands::run(command, &cwd).await
        }
        Some(Command::Agent { ref command }) => match command {
            crate::args::AgentCommand::Stdio => modes::acp::run_stdio(cli.clone()).await,
            crate::args::AgentCommand::Serve { address, token_file } => {
                modes::acp::run_serve(cli.clone(), *address, token_file.clone()).await
            }
        },
        Some(Command::Mcp { command }) => {
            let cwd = cli.cwd.clone().unwrap_or(std::env::current_dir()?);
            mcp_commands::run(command, &cwd)
        }
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
            || (cli.listen.is_none()
                && (!stdin_tty || !stdout_tty)
                && !cli.prompt.is_empty()) =>
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
                    None,
                )
                .await
            } else {
                repl::interactive(application.clone(), initial_prompts, None).await
            };
            application.cleanup().await;
            result
        }
    }
}

/// Run `--listen` as a Web-only backend. Standard input is deliberately never
/// read: a closed pipe must not stop the service, and a terminal must never be
/// acquired. The listener owns the live application until a process signal.
async fn run_listener_mode(cli: &Cli) -> Result<()> {
    let mut shutdown = ListenerShutdownSignals::new()?;
    let session_run::RunSession {
        application,
        extension_ui,
        spawner,
        ..
    } = session_run::build_session(cli).await?;
    let handle = match start_listen(cli, &application, &extension_ui, spawner).await {
        Ok(Some(handle)) => handle,
        Ok(None) => unreachable!("listener mode requires --listen"),
        Err(error) => {
            application.cleanup().await;
            return Err(error.context("starting control plane listener"));
        }
    };
    shutdown.wait().await;
    let stop_result = handle.stop().await.context("stopping control plane listener");
    application.cleanup().await;
    stop_result
}

#[cfg(unix)]
struct ListenerShutdownSignals {
    interrupt: tokio::signal::unix::Signal,
    terminate: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl ListenerShutdownSignals {
    fn new() -> Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};

        Ok(Self {
            interrupt: signal(SignalKind::interrupt()).context("installing SIGINT handler")?,
            terminate: signal(SignalKind::terminate()).context("installing SIGTERM handler")?,
        })
    }

    async fn wait(&mut self) {
        tokio::select! {
            _ = self.interrupt.recv() => {}
            _ = self.terminate.recv() => {}
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

    async fn wait(&mut self) {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// Start the opt-in `--listen` control plane.
///
/// The listener shares the Web-only mode's live [`Application`] and remains
/// active until a process signal, then stops before `application.cleanup`.
/// [`RunSessionSpawner`] builds manager-owned session runtimes for the Web
/// control plane. Bind, auth, and read failures are startup errors.
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
        plaintext: cli.listen_plaintext,
        tls_cert: cli.listen_cert.clone(),
        tls_key: cli.listen_key.clone(),
        session_factory: Some(std::sync::Arc::new(spawner)),
    };
    let handle = modes::listen::start(application.clone(), extension_ui, config).await?;
    let addr = handle.local_addr();
    // Directly openable Web UI URL: the effective advertised origin (or the
    // bound address for concrete binds) plus the `/web` route. Wildcard
    // binds without `--listen-advertised-origin` have no reachable URL.
    let web_url = handle.base_url().map(|base| format!("{base}/web"));
    let web_line = web_url.as_deref().map_or_else(String::new, |url| format!(" Web UI: {url}"));
    let auth = if cli.listen_token_file.is_some() {
        "authentication enabled"
    } else {
        "tokenless"
    };
    let scheme = if cli.listen_plaintext { "http" } else { "https" };
    eprintln!(
        "Control plane listening on {scheme}://{addr} ({auth}).{web_line}"
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
}
