//! `rpi` — Rust coding agent CLI binary.
//!
//! A thin entrypoint over the [`pi_cli`] library so integration tests can
//! drive the same code paths. Parses arguments, dispatches, and reports any
//! error on stderr without printing secret values.

use clap::Parser;

use pi_cli::Cli;

fn main() {
    #[cfg(unix)]
    {
        let arguments = std::env::args_os().skip(1).collect::<Vec<_>>();
        if arguments.as_slice()
            == [
                std::ffi::OsString::from("__sandbox-child"),
                std::ffi::OsString::from("3"),
                std::ffi::OsString::from("4"),
            ]
        {
            match pi_cli::sandbox_child_main() {
                Ok(never) => match never {},
                Err(error) => {
                    pi_cli::output::error_line(&error.to_string());
                    std::process::exit(125);
                }
            }
        }
    }

    pi_cli::harden_process();
    pi_cli::tui::install_panic_hook();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|error| {
            pi_cli::output::error_line(&format!("failed to start runtime: {error}"));
            std::process::exit(1);
        });
    let cli = Cli::parse();
    if let Err(error) = runtime.block_on(pi_cli::run(cli)) {
        pi_cli::output::error_line(&format!("{error:#}"));
        std::process::exit(1);
    }
}
