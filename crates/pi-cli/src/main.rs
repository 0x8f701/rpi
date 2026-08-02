//! `rpi` — Rust coding agent CLI binary.
//!
//! A thin entrypoint over the [`pi_cli`] library so integration tests can
//! drive the same code paths. Parses arguments, dispatches, and reports any
//! error on stderr without printing secret values.

use clap::Parser;

use pi_cli::Cli;

#[tokio::main]
async fn main() {
    // Install the terminal-restoring panic hook before any dispatch so every
    // path (TUI, RPC, JSON, print, subcommands) is covered. It is a no-op
    // outside the TUI: `TUI_ACTIVE` stays false for structured-output modes,
    // which therefore never acquire a TUI guard.
    pi_cli::tui::install_panic_hook();
    let cli = Cli::parse();
    if let Err(e) = pi_cli::run(cli).await {
        pi_cli::output::error_line(&format!("{e:#}"));
        std::process::exit(1);
    }
}
