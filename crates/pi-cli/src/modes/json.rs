use std::io::{self, Write};

use anyhow::{Context, Result};
use pi_coding::ApplicationEvent;

use crate::{args::Cli, session_run};

pub async fn run(cli: &Cli) -> Result<()> {
    let session_run::RunSession {
        application,
        todo_child_service,
        ..
    } = session_run::build_session(cli).await?;
    let result = async {
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
            let session = application.session();
            let process = application.governed_process_context();
            let expanded = crate::file_args::expand_prompt_in_workspace(
                prompt,
                session.workspace_roots(),
                Some(&process),
            )?;
            application
                .prompt(expanded.prompt, expanded.images, None)
                .await?;
            write_until_prompt_settled(&mut events, &mut output).await?;
            application.wait_for_idle().await;
        }
        Ok(())
    }
    .await;
    let shutdown = todo_child_service
        .shutdown()
        .await
        .context("shutting down Todo child service");
    application.cleanup().await;
    match (result, shutdown) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(error),
        (Err(error), Err(shutdown_error)) => Err(error.context(format!(
            "Todo child service shutdown also failed: {shutdown_error:#}"
        ))),
    }
}

async fn write_until_prompt_settled<W: Write>(
    events: &mut tokio::sync::broadcast::Receiver<ApplicationEvent>,
    output: &mut W,
) -> Result<()> {
    loop {
        let event = events
            .recv()
            .await
            .context("application event stream closed")?;
        let settled = matches!(
            event,
            ApplicationEvent::AgentSettled | ApplicationEvent::TodoChildRouted { .. }
        );
        write_json_line(output, &event)?;
        if settled {
            return Ok(());
        }
    }
}

#[doc(hidden)]
pub fn write_json_line<W: Write, T: serde::Serialize>(writer: &mut W, value: &T) -> Result<()> {
    serde_json::to_writer(&mut *writer, value).context("serializing JSONL record")?;
    writer.write_all(b"\n").context("writing JSONL delimiter")?;
    writer.flush().context("flushing JSONL record")
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_coding::{PromptMode, TodoChildReceipt, TodoChildReceiptStatus};

    fn routed_receipt() -> TodoChildReceipt {
        TodoChildReceipt {
            id: "receipt-1".to_owned(),
            parent_session_id: "parent-1".to_owned(),
            child_session_id: "child-1".to_owned(),
            message_id: "message-1".to_owned(),
            accepted_at: 1,
            status: TodoChildReceiptStatus::Queued,
            summary: None,
        }
    }

    #[tokio::test]
    async fn typed_todo_child_route_is_written_and_settles_once() {
        let (events, mut receiver) = tokio::sync::broadcast::channel(8);
        events
            .send(ApplicationEvent::TodoChildRouted {
                receipt: routed_receipt(),
            })
            .expect("route receiver");
        events
            .send(ApplicationEvent::AgentSettled)
            .expect("settled receiver");

        let mut output = Vec::new();
        write_until_prompt_settled(&mut receiver, &mut output)
            .await
            .expect("write routed prompt");

        let lines = String::from_utf8(output).expect("utf8");
        let records = lines
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("JSON event"))
            .collect::<Vec<_>>();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["type"], "todo_child_routed");
        assert_eq!(records[0]["receipt"]["id"], "receipt-1");
        assert_eq!(records[0]["receipt"]["childSessionId"], "child-1");
        assert!(matches!(
            receiver.try_recv(),
            Ok(ApplicationEvent::AgentSettled)
        ));
    }

    #[tokio::test]
    async fn ordinary_prompt_waits_for_agent_settled() {
        let (events, mut receiver) = tokio::sync::broadcast::channel(8);
        events
            .send(ApplicationEvent::ModeDetected {
                mode: PromptMode::Question,
                hint: "question".to_owned(),
            })
            .expect("mode receiver");
        events
            .send(ApplicationEvent::AgentSettled)
            .expect("settled receiver");

        let mut output = Vec::new();
        write_until_prompt_settled(&mut receiver, &mut output)
            .await
            .expect("write ordinary prompt");

        let records = String::from_utf8(output)
            .expect("utf8")
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("JSON event"))
            .collect::<Vec<_>>();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0]["type"], "mode_detected");
        assert_eq!(records[1]["type"], "agent_settled");
    }
}
