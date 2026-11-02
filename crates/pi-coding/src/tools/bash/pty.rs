//! Foreground Bash PTY execution through the captured governed process boundary.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use pi_agent::AbortSignal;

use crate::ProcessTerminalSize;
use crate::process::{ForegroundShellSpec, GovernedProcessContext};

pub(crate) const PTY_ROWS: u16 = 24;
pub(crate) const PTY_COLS: u16 = 80;

pub(crate) struct PtyRunOutput {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub cancelled: bool,
    pub execution_elapsed: Option<Duration>,
}

pub(crate) async fn run_pty_command(
    process: GovernedProcessContext,
    cwd: &Path,
    argv: &[String],
    env: &[(String, String)],
    input: Option<&str>,
    timeout: Option<Duration>,
    abort: AbortSignal,
    stream: Arc<dyn Fn(&[u8]) + Send + Sync>,
) -> Result<PtyRunOutput> {
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| anyhow!("foreground shell argv is empty"))?;
    let input = match input {
        Some(input) => {
            let mut bytes = Vec::with_capacity(input.len() + 1);
            bytes.extend_from_slice(input.as_bytes());
            bytes.push(b'\n');
            Some(bytes)
        }
        None => Some(vec![0x04]),
    };
    let outcome = process
        .run_foreground_shell(ForegroundShellSpec {
            program: program.into(),
            args: args.to_vec(),
            cwd: cwd.to_path_buf(),
            environment: env
                .iter()
                .map(|(name, value)| (name.clone(), Some(value.clone())))
                .collect::<BTreeMap<_, _>>(),
            terminal_size: ProcessTerminalSize {
                rows: PTY_ROWS,
                cols: PTY_COLS,
            },
            input,
            timeout,
            abort,
            stream,
        })
        .await?;
    Ok(PtyRunOutput {
        exit_code: outcome.exit_code,
        timed_out: outcome.timed_out,
        cancelled: outcome.cancelled,
        execution_elapsed: outcome.execution_elapsed,
    })
}
