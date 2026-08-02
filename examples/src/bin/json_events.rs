//! Consume [`pi_coding::ApplicationEvent`]s as JSON lines.
//!
//! Run from the repo root:
//!
//! ```sh
//! cargo run --bin json_events
//! ```
//!
//! The example queues one deterministic faux response, drives the session once,
//! and prints every event emitted by the application layer as a JSON line.

use anyhow::Result;
use pi_agent::ThinkingLevel;
use pi_ai::{
    providers::{register_faux_provider, FauxProviderOptions, FauxResponse},
    Model,
};
use pi_coding::{Application, Session, SessionOptions};
use std::path::PathBuf;

#[tokio::main]
async fn main() -> Result<()> {
    // Register a private faux provider so this example does not need a real key.
    let mut model = Model::default();
    model.id = "json-events-example".to_owned();
    model.name = model.id.clone();
    model.provider = "example".to_owned();
    model.api = "faux-json-events".to_owned();
    model.base_url = "http://localhost:0".to_owned();

    let faux = register_faux_provider(FauxProviderOptions {
        api: model.api.clone(),
        provider: model.provider.clone(),
        models: vec![model.clone()],
        chunk_size: 16,
    });
    faux.set_responses(vec![FauxResponse::text("Hello from JSON events.")]);

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
            println!("{}", serde_json::to_string(&event)?);
        }
        Result::<(), anyhow::Error>::Ok(())
    });

    app.prompt("Say hello".to_owned(), vec![], None).await?;
    app.wait_for_idle().await;

    // Give the printer a moment to drain, then stop waiting.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    printer.abort();

    faux.unregister();
    Ok(())
}
