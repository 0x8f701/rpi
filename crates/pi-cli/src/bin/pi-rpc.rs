use std::ffi::OsString;

use clap::{CommandFactory, FromArgMatches};
use pi_cli::Cli;

#[tokio::main]
async fn main() {
    pi_cli::tui::install_panic_hook();
    let mut args = std::env::args_os();
    let program = args.next().unwrap_or_else(|| OsString::from("pi-rpc"));
    let mut forced = vec![program];
    forced.extend(args);
    // Append after user arguments so Cli's args_override_self makes RPC mode
    // authoritative even when a caller passes a conflicting --mode value.
    forced.extend([OsString::from("--mode"), OsString::from("rpc")]);
    let matches = Cli::command().name("pi-rpc").get_matches_from(forced);
    let cli = Cli::from_arg_matches(&matches).expect("clap matches Cli schema");
    if let Err(error) = pi_cli::run(cli).await {
        pi_cli::modes::json::write_json_line(
            &mut std::io::stdout().lock(),
            &pi_cli::modes::rpc::RpcResponse::failure(None, "initialize", error.to_string()),
        )
        .ok();
        std::process::exit(1);
    }
}
