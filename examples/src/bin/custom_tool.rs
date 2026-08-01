//! Add a custom tool that shells out to an external process.
//!
//! This is the closest supported form of "extension" in this release: the
//! built-in tool set is fixed in the CLI, but library users can register
//! additional [`pi_agent::AgentTool`]s when constructing a [`pi_coding::Session`].
//!
//! Run from the repo root:
//!
//! ```sh
//! cargo run --bin custom_tool
//! ```

use anyhow::Result;
use pi_agent::{AgentTool, AgentToolResult, ToolCallContext, ThinkingLevel};
use pi_ai::{
    providers::{register_faux_provider, FauxProviderOptions, FauxResponse},
    ContentBlock, Model, Schema, StopReason, ToolCall,
};
use pi_coding::{Session, SessionOptions};
use serde_json::json;
use std::path::PathBuf;

#[tokio::main]
async fn main() -> Result<()> {
    let echo = AgentTool::new(
        "echo",
        "Echo a value using the shell",
        Schema::object(
            [("message".to_owned(), Schema::string())].into_iter().collect(),
            vec!["message".to_owned()],
        ),
        |ctx: ToolCallContext| async move {
            let message = ctx
                .arguments
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let output = tokio::process::Command::new("echo")
                .arg(message)
                .output()
                .await?;
            Ok(AgentToolResult::text(String::from_utf8_lossy(&output.stdout)))
        },
    );

    let mut model = Model::default();
    model.id = "custom-tool-example".to_owned();
    model.name = model.id.clone();
    model.provider = "example".to_owned();
    model.api = "faux-custom-tool".to_owned();
    model.base_url = "http://localhost:0".to_owned();

    let faux = register_faux_provider(FauxProviderOptions {
        api: model.api.clone(),
        provider: model.provider.clone(),
        models: vec![model.clone()],
        chunk_size: 16,
    });

    // First response asks the agent to call `echo`, second closes the turn.
    faux.set_responses(vec![
        FauxResponse {
            content: vec![ContentBlock::ToolCall(ToolCall {
                id: "call-1".to_owned(),
                name: "echo".to_owned(),
                arguments: json!({"message": "hello from custom tool"}),
                thought_signature: None,
            })],
            stop_reason: StopReason::ToolUse,
            error_message: None,
        },
        FauxResponse::text("done"),
    ]);

    let session = Session::new(SessionOptions {
        model,
        cwd: PathBuf::from("."),
        system_prompt: String::new(),
        thinking_level: ThinkingLevel::Off,
        api_key: "faux".to_owned(),
        compaction: None,
        stream_options: Default::default(),
        tools: Some(vec![echo]),
        before_tool_call: None,
        after_tool_call: None,
        stream_fn: None,
        auth_resolver: None,
    })?;

    let mut stdout = std::io::stdout().lock();
    let _text = session.run_print(&mut stdout, "Call echo").await?;

    faux.unregister();
    Ok(())
}
