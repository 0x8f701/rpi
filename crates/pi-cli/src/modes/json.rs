use std::io::{self, Write};

use anyhow::{Context, Result};
use pi_coding::ApplicationEvent;

use crate::{args::Cli, session_run};

pub async fn run(cli: &Cli) -> Result<()> {
    let session_run::RunSession { application, .. } = session_run::build_session(cli).await?;
    let mut events = application.subscribe();
    let stdout = io::stdout();
    let mut output = stdout.lock();

    let header = application
        .session_header()
        .context("session recording is unavailable")?;
    write_json_line(&mut output, &header)?;

    for prompt in &cli.prompt {
        if prompt.is_empty() {
            continue;
        }
        let cwd = application.session().cwd().to_path_buf();
        let expanded = crate::file_args::expand_prompt(prompt, &cwd)?;
        application
            .prompt(expanded.prompt, expanded.images, None)
            .await?;
        loop {
            let event = events
                .recv()
                .await
                .context("application event stream closed")?;
            let settled = matches!(event, ApplicationEvent::AgentSettled);
            write_json_line(&mut output, &event)?;
            if settled {
                break;
            }
        }
        application.wait_for_idle().await;
    }
    application.cleanup().await;
    Ok(())
}

#[doc(hidden)]
pub fn write_json_line<W: Write, T: serde::Serialize>(writer: &mut W, value: &T) -> Result<()> {
    serde_json::to_writer(&mut *writer, value).context("serializing JSONL record")?;
    writer.write_all(b"\n").context("writing JSONL delimiter")?;
    writer.flush().context("flushing JSONL record")
}
