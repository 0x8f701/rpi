use std::sync::Arc;

pub use pi_agent::compose_before_tool_call;
use pi_agent::{
    ApprovalMode, BeforeToolCallContext, BeforeToolCallFn, BeforeToolCallResult, ToolCapability,
};
use pi_coding::ExtensionMode;

use crate::extension_ui::ExtensionUiAdapter;

const DENIED_REASON: &str = "Tool execution denied by host approval policy";
const UNAVAILABLE_REASON: &str =
    "Tool execution blocked: host approval is required but no interactive confirmation adapter is available";

#[must_use]
pub fn host_approval_before_tool_call(
    mode: ApprovalMode,
    extension_mode: ExtensionMode,
    adapter: Option<ExtensionUiAdapter>,
    existing: Option<BeforeToolCallFn>,
) -> BeforeToolCallFn {
    let approval: BeforeToolCallFn = Arc::new(move |context| {
        let adapter = adapter.clone();
        Box::pin(async move {
            let capability = tool_capability(&context);
            if !mode.requires_approval(capability) {
                return Ok(BeforeToolCallResult::default());
            }
            let Some(adapter) = adapter else {
                return Ok(blocked(UNAVAILABLE_REASON));
            };
            match adapter
                .confirm_host_tool(extension_mode, &context.tool_call.name, capability)
                .await
            {
                Ok(crate::extension_ui::HostToolConfirmation::Approved) => {
                    Ok(BeforeToolCallResult::default())
                }
                Ok(crate::extension_ui::HostToolConfirmation::Denied) => Ok(blocked(DENIED_REASON)),
                Ok(crate::extension_ui::HostToolConfirmation::Cancelled) => Ok(blocked(
                    "Tool execution cancelled by host approval policy",
                )),
                Err(error) => Ok(blocked(format!(
                    "Tool execution blocked: host approval failed: {error}"
                ))),
            }
        })
    });
    compose_before_tool_call(Some(approval), existing).expect("approval hook is always present")
}

fn tool_capability(context: &BeforeToolCallContext) -> ToolCapability {
    context
        .context
        .tools
        .iter()
        .find(|tool| tool.name == context.tool_call.name)
        .map_or_else(ToolCapability::default, |tool| tool.capability)
}

fn blocked(reason: impl Into<String>) -> BeforeToolCallResult {
    BeforeToolCallResult {
        block: true,
        reason: Some(reason.into()),
        arguments: None,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use pi_agent::{AgentContext, AgentTool, AgentToolResult};
    use pi_ai::{AssistantMessage, Model, Schema, ToolCall};
    use serde_json::json;

    use super::*;

    fn context(tool_name: &str, capability: ToolCapability) -> BeforeToolCallContext {
        let tool = AgentTool::new(tool_name, "test", Schema::default(), |_| async {
            Ok(AgentToolResult::text("ok"))
        })
        .with_capability(capability);
        BeforeToolCallContext {
            assistant_message: AssistantMessage::pending(&Model::default()),
            tool_call: ToolCall {
                id: "call-1".to_owned(),
                name: tool_name.to_owned(),
                arguments: json!({}),
                thought_signature: None,
            },
            arguments: json!({}),
            context: AgentContext {
                system_prompt: String::new(),
                messages: Vec::new(),
                tools: vec![tool],
            },
        }
    }

    async fn answer_confirmation(
        mut events: tokio::sync::broadcast::Receiver<crate::extension_ui::ExtensionUiEvent>,
        adapter: ExtensionUiAdapter,
        response: HostResponse,
    ) {
        let event = events.recv().await.expect("approval interaction");
        let crate::extension_ui::ExtensionUiEvent::InteractionRequested { interaction } = event
        else {
            panic!("expected interaction request")
        };
        match response {
            HostResponse::Allow => adapter.respond_confirmed(&interaction.id, true).unwrap(),
            HostResponse::Deny => adapter.respond_confirmed(&interaction.id, false).unwrap(),
            HostResponse::Cancel => adapter.cancel(&interaction.id).unwrap(),
        }
    }

    #[derive(Clone, Copy)]
    enum HostResponse {
        Allow,
        Deny,
        Cancel,
    }

    #[tokio::test]
    async fn interactive_confirmation_allows_denies_and_cancels() {
        for (response, blocked, reason_fragment) in [
            (HostResponse::Allow, false, None),
            (HostResponse::Deny, true, Some("denied")),
            (HostResponse::Cancel, true, Some("cancelled")),
        ] {
            let adapter = ExtensionUiAdapter::new();
            let events = adapter.subscribe();
            let responder = tokio::spawn(answer_confirmation(events, adapter.clone(), response));
            let hook = host_approval_before_tool_call(
                ApprovalMode::Ask,
                ExtensionMode::Tui,
                Some(adapter),
                None,
            );
            let result = hook(context("read", ToolCapability::Read)).await.unwrap();
            responder.await.unwrap();
            assert_eq!(result.block, blocked);
            if let Some(fragment) = reason_fragment {
                assert!(result.reason.as_deref().is_some_and(|reason| reason.contains(fragment)));
            }
        }
    }

    #[tokio::test]
    async fn headless_write_and_ask_fail_closed_when_policy_requires_confirmation() {
        for (mode, capability) in [
            (ApprovalMode::Write, ToolCapability::Exec),
            (ApprovalMode::Ask, ToolCapability::Read),
        ] {
            let hook = host_approval_before_tool_call(mode, ExtensionMode::Print, None, None);
            let result = hook(context("tool", capability)).await.unwrap();
            assert!(result.block);
            assert!(result.reason.unwrap().contains("no interactive confirmation adapter"));
        }
    }

    #[tokio::test]
    async fn allowed_calls_invoke_existing_hook_and_preserve_its_argument_rewrite() {
        let calls = Arc::new(AtomicUsize::new(0));
        let hook_calls = calls.clone();
        let existing: BeforeToolCallFn = Arc::new(move |_| {
            hook_calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                Ok(BeforeToolCallResult {
                    block: false,
                    reason: None,
                    arguments: Some(json!({"rewritten": true})),
                })
            })
        });
        let hook = host_approval_before_tool_call(
            ApprovalMode::Yolo,
            ExtensionMode::Print,
            None,
            Some(existing),
        );
        let result = hook(context("read", ToolCapability::Read)).await.unwrap();
        assert!(!result.block);
        assert_eq!(result.arguments, Some(json!({"rewritten": true})));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn compose_helper_orders_host_before_extension_style_reducer() {
        let order = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let first_order = order.clone();
        let first: BeforeToolCallFn = Arc::new(move |_| {
            first_order.lock().push("host");
            Box::pin(async { Ok(BeforeToolCallResult::default()) })
        });
        let second_order = order.clone();
        let second: BeforeToolCallFn = Arc::new(move |_| {
            second_order.lock().push("extension");
            Box::pin(async { Ok(BeforeToolCallResult::default()) })
        });
        let composed = compose_before_tool_call(Some(first), Some(second)).expect("composed");
        assert!(!composed(context("read", ToolCapability::Read)).await.unwrap().block);
        assert_eq!(&*order.lock(), &["host", "extension"]);
    }

    #[tokio::test]
    async fn read_named_generic_tool_is_exec_and_denial_skips_existing_hook() {
        let calls = Arc::new(AtomicUsize::new(0));
        let hook_calls = calls.clone();
        let existing: BeforeToolCallFn = Arc::new(move |_| {
            hook_calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(BeforeToolCallResult::default()) })
        });
        let adapter = ExtensionUiAdapter::new();
        let events = adapter.subscribe();
        let responder = tokio::spawn(answer_confirmation(events, adapter.clone(), HostResponse::Deny));
        let hook = host_approval_before_tool_call(
            ApprovalMode::Write,
            ExtensionMode::Rpc,
            Some(adapter),
            Some(existing),
        );
        let result = hook(context("read", ToolCapability::Exec)).await.unwrap();

        responder.await.unwrap();
        assert!(result.block);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
    #[tokio::test]
    async fn missing_tool_metadata_defaults_to_exec_without_name_inference() {
        let hook = host_approval_before_tool_call(
            ApprovalMode::Write,
            ExtensionMode::Print,
            None,
            None,
        );
        let mut context = context("read", ToolCapability::Read);
        context.context.tools.clear();
        let result = hook(context).await.unwrap();
        assert!(result.block);
        assert!(result.reason.unwrap().contains("no interactive confirmation adapter"));
    }

    #[tokio::test]
    async fn broker_errors_fail_closed_and_skip_existing_hook() {
        let calls = Arc::new(AtomicUsize::new(0));
        let hook_calls = calls.clone();
        let existing: BeforeToolCallFn = Arc::new(move |_| {
            hook_calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(BeforeToolCallResult::default()) })
        });
        let hook = host_approval_before_tool_call(
            ApprovalMode::Ask,
            ExtensionMode::Tui,
            Some(ExtensionUiAdapter::new()),
            Some(existing),
        );
        let result = hook(context("write", ToolCapability::Write)).await.unwrap();
        assert!(result.block);
        assert!(result.reason.unwrap().contains("host approval failed"));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
}
