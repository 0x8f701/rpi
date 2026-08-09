//! The interactive `ask` tool factory.
//!
//! Two construction flavors share the same pi-agent tool definition
//! (schema `{question: string}`, `Read` capability):
//! - [`session_ask_tool`] binds a live [`crate::AskRuntime`] so the round trip
//!   publishes `SessionEvent::AskUser` and awaits the user's answer. Sessions
//!   use this.
//! - [`standalone_ask_tool`] always rejects with the non-interactive error.
//!   Standalone factories (`create_all_tools`, `create_tool("ask", cwd)`, ...)
//!   use this: there is no frontend to answer the question.

use std::sync::Arc;

use pi_agent::{AgentTool, create_ask_tool};

use crate::AskRuntime;

/// The `ask` tool wired to a live session ask slot.
pub(crate) fn session_ask_tool(runtime: AskRuntime) -> AgentTool {
    create_ask_tool(Arc::new(move |question, abort| {
        let runtime = runtime.clone();
        Box::pin(async move { runtime.request(question, abort).await })
    }))
}

/// The `ask` tool without any frontend binding: always rejects with the
/// actionable non-interactive error so a model never hangs on a question
/// nobody can answer.
pub(crate) fn standalone_ask_tool() -> AgentTool {
    create_ask_tool(Arc::new(|question, _abort| {
        Box::pin(async move {
            Err(anyhow::anyhow!(
                "ask requires an interactive session; this mode cannot prompt the user \
                 (question was: {question})"
            ))
        })
    }))
}
