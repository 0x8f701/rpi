//! Minimal JSON-RPC-style client over the in-process [`pi_coding::Application`] event stream.
//!
//! Run from the repo root:
//!
//! ```sh
//! cargo run --bin rpc_client
//! ```
//!
//! The CLI also exposes the production LF-delimited stdio protocol through
//! `rpi --mode rpc`. This example stays in-process to demonstrate subscribing to
//! application events and wrapping them as JSON-RPC-style notifications.

use anyhow::Result;
use pi_agent::ThinkingLevel;
use pi_ai::{
    providers::{register_faux_provider, FauxProviderOptions, FauxResponse},
    Model,
};
use pi_coding::{Application, Session, SessionOptions};
use serde_json::json;
use std::path::PathBuf;

#[tokio::main]
async fn main() -> Result<()> {
    let mut model = Model::default();
    model.id = "rpc-client-example".to_owned();
    model.name = model.id.clone();
    model.provider = "example".to_owned();
    model.api = "faux-rpc-client".to_owned();
    model.base_url = "http://localhost:0".to_owned();

    let faux = register_faux_provider(FauxProviderOptions {
        api: model.api.clone(),
        provider: model.provider.clone(),
        models: vec![model.clone()],
        chunk_size: 16,
    });
    faux.set_responses(vec![FauxResponse::text("RPC-style notification")]);

    let session = Session::new(SessionOptions {
        model,
        cwd: PathBuf::from("."),
        system_prompt: String::new(),
        thinking_level: ThinkingLevel::Off,
        api_key: "faux".to_owned(),
        compaction: None,
        stream_options: Default::default(),
        tools: None,
        before_tool_call: None,
        after_tool_call: None,
        stream_fn: None,
        auth_resolver: None,
    })?;

    let app = Application::new(session).await;
    let mut events = app.subscribe();

    let printer = tokio::spawn(async move {
        while let Ok(event) = events.recv().await {
            let envelope = json!({
                "jsonrpc": "2.0",
                "method": format!("rpi.{}" , event_type(&event)),
                "params": event,
            });
            println!("{}", serde_json::to_string(&envelope)?);
        }
        Result::<(), anyhow::Error>::Ok(())
    });

    app.prompt("Say hello".to_owned(), vec![], None).await?;
    app.wait_for_idle().await;

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    printer.abort();

    faux.unregister();
    Ok(())
}

fn event_type(event: &pi_coding::ApplicationEvent) -> String {
    use pi_coding::ApplicationEvent;
    match event {
        ApplicationEvent::RuntimeChanged { .. } => "runtimeChanged",
        ApplicationEvent::SessionStarted(_) => "sessionStarted",
        ApplicationEvent::Session(_) => "session",
        ApplicationEvent::Agent(_) => "agent",
        ApplicationEvent::RunFailed { .. } => "runFailed",
        ApplicationEvent::AgentSettled => "agentSettled",
        ApplicationEvent::Exported { .. } => "exported",
        ApplicationEvent::ShareSucceeded { .. } => "shareSucceeded",
        ApplicationEvent::ShareFailed { .. } => "shareFailed",
        ApplicationEvent::SessionBeforeTree(_) => "sessionBeforeTree",
        ApplicationEvent::SessionTree(_) => "sessionTree",
        ApplicationEvent::SessionBeforeFork(_) => "sessionBeforeFork",
        ApplicationEvent::SessionForked(_) => "sessionForked",
        ApplicationEvent::Process(_) => "process",
        ApplicationEvent::Selection(_) => "selection",
        ApplicationEvent::Loop(_) => "loop",
        ApplicationEvent::Orchestration(_) => "orchestration",
        ApplicationEvent::Workflow(_) => "workflow",
        ApplicationEvent::TodoUpdated { .. } => "todoUpdated",
        ApplicationEvent::TodoReminder { .. } => "todoReminder",
        ApplicationEvent::GoalUpdated { .. } => "goalUpdated",
        ApplicationEvent::GoalUsageCharged { .. } => "goalUsageCharged",
        ApplicationEvent::GoalContinuation { .. } => "goalContinuation",
    }
    .to_owned()
}
