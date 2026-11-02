use std::path::PathBuf;
use std::sync::Arc;

pub use pi_agent::compose_before_tool_call;
use pi_agent::{
    ApprovalMode, BeforeToolCallContext, BeforeToolCallFn, BeforeToolCallResult, ToolCapability,
};
use pi_coding::{ExtensionMode, PermissionRule, PermissionVerdict, permission_verdict};

use crate::extension_ui::ExtensionUiAdapter;

const DENIED_REASON: &str = "Tool execution denied by host approval policy";
const UNAVAILABLE_REASON: &str = "Tool execution blocked: host approval is required but no interactive confirmation adapter is available";

/// Live source of path-level permission rules.
///
/// Consulted on every file-touching tool call so `permissionRules` changes
/// take effect without a session restart (RELOAD apply behavior in the
/// settings catalog).
pub type PermissionRulesSource = Arc<dyn Fn() -> Vec<PermissionRule> + Send + Sync>;

/// A rules source that never matches; used where no path policy is configured.
#[must_use]
pub fn empty_permission_rules() -> PermissionRulesSource {
    Arc::new(Vec::new)
}
/// Compose host/final approval reducers, then issue the sandbox token only
/// after the composed decision is allowed.
///
/// Final approval has a distinct boundary: policy and downstream hooks run
/// first, downstream hooks cannot consume the prepared request, and that
/// request is consumed exactly once only when the aggregate outcome proceeds.
pub(crate) fn compose_final_approval(
    approval: BeforeToolCallFn,
    existing: Option<BeforeToolCallFn>,
) -> BeforeToolCallFn {
    Arc::new(move |context| {
        let approval = approval.clone();
        let existing = existing.clone();
        Box::pin(async move {
            let approval_request = context.approval_request.clone();
            let first_outcome = approval(context.clone()).await?;
            let outcome = if first_outcome.block {
                first_outcome
            } else if let Some(existing) = existing {
                let mut next_context = context;
                next_context.approval_request = None;
                if let Some(arguments) = first_outcome.arguments.clone() {
                    next_context.arguments = arguments;
                }
                let second_outcome = existing(next_context).await?;
                BeforeToolCallResult {
                    block: second_outcome.block,
                    reason: second_outcome.reason.or(first_outcome.reason),
                    arguments: second_outcome.arguments.or(first_outcome.arguments),
                    approval_token: None,
                }
            } else {
                first_outcome
            };
            issue_final_approval(approval_request.as_ref(), outcome).await
        })
    })
}

/// Single issuance authority for prepared sandbox approval requests.
pub(crate) async fn issue_final_approval(
    approval_request: Option<&pi_agent::ToolApprovalRequest>,
    mut outcome: BeforeToolCallResult,
) -> anyhow::Result<BeforeToolCallResult> {
    outcome.approval_token = None;
    if outcome.block {
        return Ok(outcome);
    }
    let Some(request) = approval_request else {
        return Ok(outcome);
    };
    if outcome.arguments.is_some() {
        anyhow::bail!("final approval hook cannot rewrite tool arguments");
    }
    outcome.approval_token = Some(request.issue().await?);
    Ok(outcome)
}

#[must_use]
pub fn host_approval_before_tool_call(
    mode: ApprovalMode,
    extension_mode: ExtensionMode,
    adapter: Option<ExtensionUiAdapter>,
    existing: Option<BeforeToolCallFn>,
    cwd: PathBuf,
    permission_rules: PermissionRulesSource,
) -> BeforeToolCallFn {
    let approval: BeforeToolCallFn = Arc::new(move |context| {
        let adapter = adapter.clone();
        let permission_rules = permission_rules.clone();
        let cwd = cwd.clone();
        Box::pin(async move {
            // Path-level permission rules run BEFORE the capability decision:
            // deny blocks outright, allow bypasses the capability ask, and ask
            // forces interactive confirmation even when the mode would allow.
            // Bash and other exec tools are not rule-addressable (no reliable
            // target path) and fall through to the capability decision.
            let verdict = permission_verdict(
                &context.tool_call.name,
                &context.arguments,
                &cwd,
                &permission_rules(),
            );
            let forced_ask = matches!(&verdict, PermissionVerdict::Ask);
            match verdict {
                PermissionVerdict::Deny(reason) => return Ok(blocked(reason)),
                PermissionVerdict::Allow => return Ok(BeforeToolCallResult::default()),
                PermissionVerdict::Ask | PermissionVerdict::NoMatch => {}
            }
            let capability = tool_capability(&context);
            if !forced_ask && !mode.requires_approval(capability) {
                return Ok(BeforeToolCallResult::default());
            }
            let Some(adapter) = adapter else {
                return Ok(blocked(UNAVAILABLE_REASON));
            };
            match adapter
                .confirm_host_tool(
                    extension_mode,
                    &context.tool_call.name,
                    capability,
                    context
                        .approval_request
                        .as_ref()
                        .map(|request| &request.summary),
                )
                .await
            {
                Ok(crate::extension_ui::HostToolConfirmation::Approved) => {
                    Ok(BeforeToolCallResult::default())
                }
                Ok(crate::extension_ui::HostToolConfirmation::Denied) => Ok(blocked(DENIED_REASON)),
                Ok(crate::extension_ui::HostToolConfirmation::Cancelled) => {
                    Ok(blocked("Tool execution cancelled by host approval policy"))
                }
                Err(error) => Ok(blocked(format!(
                    "Tool execution blocked: host approval failed: {error}"
                ))),
            }
        })
    });
    compose_final_approval(approval, existing)
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
        approval_token: None,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use pi_agent::{
        AgentContext, AgentTool, AgentToolResult, ToolApprovalDigest, ToolApprovalId,
        ToolApprovalRequest, ToolApprovalSummary, ToolCallApprovalToken,
    };
    use pi_ai::{AssistantMessage, Model, Schema, ToolCall};
    use pi_coding::PermissionRule;
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
            approval_request: None,
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
                PathBuf::new(),
                empty_permission_rules(),
            );
            let result = hook(context("read", ToolCapability::Read)).await.unwrap();
            responder.await.unwrap();
            assert_eq!(result.block, blocked);
            if let Some(fragment) = reason_fragment {
                assert!(
                    result
                        .reason
                        .as_deref()
                        .is_some_and(|reason| reason.contains(fragment))
                );
            }
        }
    }

    #[tokio::test]
    async fn headless_write_and_ask_fail_closed_when_policy_requires_confirmation() {
        for (mode, capability) in [
            (ApprovalMode::Write, ToolCapability::Exec),
            (ApprovalMode::Ask, ToolCapability::Read),
        ] {
            let hook = host_approval_before_tool_call(
                mode,
                ExtensionMode::Print,
                None,
                None,
                PathBuf::new(),
                empty_permission_rules(),
            );
            let result = hook(context("tool", capability)).await.unwrap();
            assert!(result.block);
            assert!(
                result
                    .reason
                    .unwrap()
                    .contains("no interactive confirmation adapter")
            );
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
                    approval_token: None,
                })
            })
        });
        let hook = host_approval_before_tool_call(
            ApprovalMode::Yolo,
            ExtensionMode::Print,
            None,
            Some(existing),
            PathBuf::new(),
            empty_permission_rules(),
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
        assert!(
            !composed(context("read", ToolCapability::Read))
                .await
                .unwrap()
                .block
        );
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
        let responder = tokio::spawn(answer_confirmation(
            events,
            adapter.clone(),
            HostResponse::Deny,
        ));
        let hook = host_approval_before_tool_call(
            ApprovalMode::Write,
            ExtensionMode::Rpc,
            Some(adapter),
            Some(existing),
            PathBuf::new(),
            empty_permission_rules(),
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
            PathBuf::new(),
            empty_permission_rules(),
        );
        let mut context = context("read", ToolCapability::Read);
        context.context.tools.clear();
        let result = hook(context).await.unwrap();
        assert!(result.block);
        assert!(
            result
                .reason
                .unwrap()
                .contains("no interactive confirmation adapter")
        );
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
            PathBuf::new(),
            empty_permission_rules(),
        );
        let result = hook(context("write", ToolCapability::Write)).await.unwrap();
        assert!(result.block);
        assert!(result.reason.unwrap().contains("host approval failed"));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    fn context_with_args(
        tool_name: &str,
        capability: ToolCapability,
        args: serde_json::Value,
    ) -> BeforeToolCallContext {
        let mut context = context(tool_name, capability);
        context.tool_call.arguments = args.clone();
        context.arguments = args;
        context
    }

    fn rule(action: pi_coding::PermissionRuleAction, path: &str) -> PermissionRule {
        PermissionRule {
            action,
            path: path.to_owned(),
            tools: None,
            extra: Default::default(),
        }
    }

    fn rules_source(rules: Vec<PermissionRule>) -> PermissionRulesSource {
        Arc::new(move || rules.clone())
    }

    #[tokio::test]
    async fn deny_rule_blocks_read_with_actionable_message() {
        let hook = host_approval_before_tool_call(
            ApprovalMode::Yolo,
            ExtensionMode::Print,
            None,
            None,
            PathBuf::from("/"),
            rules_source(vec![rule(pi_coding::PermissionRuleAction::Deny, "/secret")]),
        );
        let result = hook(context_with_args(
            "read",
            ToolCapability::Read,
            json!({"path": "/secret/data.txt"}),
        ))
        .await
        .unwrap();
        assert!(result.block);
        let reason = result.reason.expect("denial reason");
        assert!(
            reason.contains("denied by path permission rule"),
            "{reason}"
        );
        assert!(reason.contains("/secret"), "{reason}");
    }

    #[tokio::test]
    async fn deny_rule_beats_allow_rule_on_same_path() {
        let hook = host_approval_before_tool_call(
            ApprovalMode::Yolo,
            ExtensionMode::Print,
            None,
            None,
            PathBuf::from("/"),
            rules_source(vec![
                rule(pi_coding::PermissionRuleAction::Allow, "/secret"),
                rule(pi_coding::PermissionRuleAction::Deny, "/secret"),
            ]),
        );
        let result = hook(context_with_args(
            "read",
            ToolCapability::Read,
            json!({"path": "/secret/data.txt"}),
        ))
        .await
        .unwrap();
        assert!(result.block, "deny must beat allow at equal specificity");
    }

    #[tokio::test]
    async fn allow_rule_bypasses_ask_and_ask_rule_forces_confirmation_in_write_mode() {
        // Write mode never asks for Write-capability tools, so the allow rule
        // is observable by the absence of a confirmation and the ask rule by
        // the forced confirmation on another path.
        let adapter = ExtensionUiAdapter::new();
        let events = adapter.subscribe();
        let responder = tokio::spawn(answer_confirmation(
            events,
            adapter.clone(),
            HostResponse::Deny,
        ));
        let hook = host_approval_before_tool_call(
            ApprovalMode::Write,
            ExtensionMode::Tui,
            Some(adapter),
            None,
            PathBuf::from("/"),
            rules_source(vec![
                rule(pi_coding::PermissionRuleAction::Allow, "/safe"),
                rule(pi_coding::PermissionRuleAction::Ask, "/elsewhere"),
            ]),
        );

        let allowed = hook(context_with_args(
            "write",
            ToolCapability::Write,
            json!({"path": "/safe/out.txt", "content": "x"}),
        ))
        .await
        .unwrap();
        assert!(
            !allowed.block,
            "allow rule must bypass the capability decision"
        );

        let forced_ask = hook(context_with_args(
            "write",
            ToolCapability::Write,
            json!({"path": "/elsewhere/out.txt", "content": "x"}),
        ))
        .await
        .unwrap();
        responder.await.unwrap();
        assert!(
            forced_ask.block,
            "ask rule must force confirmation in write mode"
        );
        assert!(
            forced_ask
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("denied")),
            "{:?}",
            forced_ask.reason
        );
    }

    #[tokio::test]
    async fn ask_rule_forces_prompt_in_yolo_mode() {
        let adapter = ExtensionUiAdapter::new();
        let events = adapter.subscribe();
        let responder = tokio::spawn(answer_confirmation(
            events,
            adapter.clone(),
            HostResponse::Allow,
        ));
        let hook = host_approval_before_tool_call(
            ApprovalMode::Yolo,
            ExtensionMode::Tui,
            Some(adapter),
            None,
            PathBuf::from("/"),
            rules_source(vec![rule(pi_coding::PermissionRuleAction::Ask, "/project")]),
        );
        let allowed = hook(context_with_args(
            "read",
            ToolCapability::Read,
            json!({"path": "/project/a.txt"}),
        ))
        .await
        .unwrap();
        responder.await.unwrap();
        assert!(!allowed.block, "confirmed ask rule allows the call");

        let adapter = ExtensionUiAdapter::new();
        let events = adapter.subscribe();
        let responder = tokio::spawn(answer_confirmation(
            events,
            adapter.clone(),
            HostResponse::Deny,
        ));
        let hook = host_approval_before_tool_call(
            ApprovalMode::Yolo,
            ExtensionMode::Tui,
            Some(adapter),
            None,
            PathBuf::from("/"),
            rules_source(vec![rule(pi_coding::PermissionRuleAction::Ask, "/project")]),
        );
        let denied = hook(context_with_args(
            "read",
            ToolCapability::Read,
            json!({"path": "/project/b.txt"}),
        ))
        .await
        .unwrap();
        responder.await.unwrap();
        assert!(denied.block);
    }

    #[tokio::test]
    async fn unmatched_path_falls_through_to_capability_mode() {
        // Ask mode without an adapter fails closed for unmatched paths…
        let hook = host_approval_before_tool_call(
            ApprovalMode::Ask,
            ExtensionMode::Print,
            None,
            None,
            PathBuf::from("/"),
            rules_source(vec![rule(pi_coding::PermissionRuleAction::Allow, "/safe")]),
        );
        let unmatched = hook(context_with_args(
            "read",
            ToolCapability::Read,
            json!({"path": "/elsewhere/a.txt"}),
        ))
        .await
        .unwrap();
        assert!(
            unmatched.block,
            "unmatched path must fall through to the capability decision"
        );
        assert!(
            unmatched
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("no interactive confirmation adapter"))
        );

        // …while the allow rule bypasses it for its own path.
        let allowed = hook(context_with_args(
            "read",
            ToolCapability::Read,
            json!({"path": "/safe/a.txt"}),
        ))
        .await
        .unwrap();
        assert!(!allowed.block, "allow rule must bypass the capability ask");
    }

    #[tokio::test]
    async fn relative_rule_paths_resolve_against_session_cwd() {
        let cwd = tempfile::tempdir().expect("cwd");
        let hook = host_approval_before_tool_call(
            ApprovalMode::Yolo,
            ExtensionMode::Print,
            None,
            None,
            cwd.path().to_path_buf(),
            rules_source(vec![rule(pi_coding::PermissionRuleAction::Deny, "secret")]),
        );
        let result = hook(context_with_args(
            "read",
            ToolCapability::Read,
            json!({"path": "secret/data.txt"}),
        ))
        .await
        .unwrap();
        assert!(
            result.block,
            "relative rule must resolve against the session cwd"
        );
    }

    #[tokio::test]
    async fn bash_is_not_covered_by_path_rules() {
        let hook = host_approval_before_tool_call(
            ApprovalMode::Yolo,
            ExtensionMode::Print,
            None,
            None,
            PathBuf::from("/"),
            rules_source(vec![rule(pi_coding::PermissionRuleAction::Deny, "/")]),
        );
        let result = hook(context_with_args(
            "bash",
            ToolCapability::Exec,
            json!({"command": "echo hi"}),
        ))
        .await
        .unwrap();
        assert!(
            !result.block,
            "bash is outside path-rule scope; yolo must allow it"
        );
    }

    #[tokio::test]
    async fn rules_are_read_live_per_tool_call() {
        let shared = Arc::new(std::sync::Mutex::new(vec![rule(
            pi_coding::PermissionRuleAction::Deny,
            "/secret",
        )]));
        let source: PermissionRulesSource = {
            let shared = shared.clone();
            Arc::new(move || shared.lock().expect("rules lock").clone())
        };
        let hook = host_approval_before_tool_call(
            ApprovalMode::Yolo,
            ExtensionMode::Print,
            None,
            None,
            PathBuf::from("/"),
            source,
        );
        let context = || {
            context_with_args(
                "read",
                ToolCapability::Read,
                json!({"path": "/secret/data.txt"}),
            )
        };
        let blocked = hook(context()).await.unwrap();
        assert!(blocked.block);

        shared.lock().expect("rules lock").clear();
        let allowed = hook(context()).await.unwrap();
        assert!(!allowed.block, "rules must be re-read on every call");
    }

    // ---- Approval-token issuance matrix ----
    //
    // `issue_final_approval` is the single authority that mints the final
    // sandbox authorization token. Allow/Yolo automatic decisions issue the
    // token when a request is present; Ask issues it only after explicit
    // confirmation; deny/cancel/unavailable issue none and leave the request
    // unissued.

    fn matrix_token() -> ToolCallApprovalToken {
        let mut bytes = [0_u8; 16];
        bytes[6] = 0x70;
        bytes[8] = 0x80;
        ToolCallApprovalToken::new(
            ToolApprovalId::from_uuid_v7_bytes(bytes).expect("uuid v7"),
            ToolApprovalDigest::from_sha256([7_u8; 32]),
        )
    }

    fn matrix_request() -> ToolApprovalRequest {
        let expected = matrix_token();
        ToolApprovalRequest::new(
            ToolApprovalSummary {
                family: "browser".to_owned(),
                executable: "governed browser executable".to_owned(),
                argument_count: 0,
                working_directory: "captured workspace".to_owned(),
                credential_audience: Some("browser".to_owned()),
                requested_credential_count: 0,
                target_fds: Vec::new(),
            },
            Box::new(move || Box::pin(async move { Ok(expected) })),
        )
    }

    #[tokio::test]
    async fn allow_rule_issues_final_token_when_request_present() {
        let request = matrix_request();
        let hook = host_approval_before_tool_call(
            ApprovalMode::Yolo,
            ExtensionMode::Print,
            None,
            None,
            PathBuf::from("/"),
            rules_source(vec![rule(pi_coding::PermissionRuleAction::Allow, "/safe")]),
        );
        let context =
            context_with_args("read", ToolCapability::Read, json!({"path": "/safe/a.txt"}));
        let mut context = context;
        context.approval_request = Some(request.clone());
        let outcome = hook(context).await.unwrap();
        assert!(!outcome.block, "allow rule must not block");
        let token = outcome
            .approval_token
            .expect("automatic allow must issue the final sandbox token");
        assert_eq!(token, matrix_token());
        // The request was consumed by the single issue path.
        assert!(
            request.issue().await.is_err(),
            "token must be minted exactly once"
        );
    }

    #[tokio::test]
    async fn allow_rule_issues_no_token_without_request() {
        // When consulted as a general before-tool-call hook (approval_request
        // is None there), the automatic-allow path must not issue a token —
        // general hooks forbid tokens (loop_runtime.rs).
        let hook = host_approval_before_tool_call(
            ApprovalMode::Yolo,
            ExtensionMode::Print,
            None,
            None,
            PathBuf::from("/"),
            rules_source(vec![rule(pi_coding::PermissionRuleAction::Allow, "/safe")]),
        );
        let outcome = hook(context_with_args(
            "read",
            ToolCapability::Read,
            json!({"path": "/safe/a.txt"}),
        ))
        .await
        .unwrap();
        assert!(!outcome.block);
        assert!(outcome.approval_token.is_none());
    }

    #[tokio::test]
    async fn composed_allow_issues_token_after_downstream_allow() {
        let downstream: BeforeToolCallFn = Arc::new(|context| {
            assert!(
                context.approval_request.is_none(),
                "only the final issuance boundary may receive the prepared request"
            );
            Box::pin(async { Ok(BeforeToolCallResult::default()) })
        });
        let request = matrix_request();
        let hook = host_approval_before_tool_call(
            ApprovalMode::Yolo,
            ExtensionMode::Print,
            None,
            Some(downstream),
            PathBuf::from("/"),
            rules_source(vec![rule(pi_coding::PermissionRuleAction::Allow, "/safe")]),
        );
        let mut approval_context =
            context_with_args("read", ToolCapability::Read, json!({"path": "/safe/a.txt"}));
        approval_context.approval_request = Some(request.clone());

        let outcome = hook(approval_context).await.unwrap();

        assert!(!outcome.block);
        assert_eq!(
            outcome
                .approval_token
                .expect("final allow must retain issued token"),
            matrix_token()
        );
        assert!(
            request.issue().await.is_err(),
            "token must be issued exactly once"
        );
    }

    #[tokio::test]
    async fn composed_downstream_denial_leaves_request_unissued() {
        let downstream: BeforeToolCallFn = Arc::new(|context| {
            assert!(
                context.approval_request.is_none(),
                "only the final issuance boundary may receive the prepared request"
            );
            Box::pin(async {
                Ok(BeforeToolCallResult {
                    block: true,
                    reason: Some("downstream denied".to_owned()),
                    ..BeforeToolCallResult::default()
                })
            })
        });
        let request = matrix_request();
        let hook = host_approval_before_tool_call(
            ApprovalMode::Yolo,
            ExtensionMode::Print,
            None,
            Some(downstream),
            PathBuf::new(),
            empty_permission_rules(),
        );
        let mut approval_context = context("read", ToolCapability::Read);
        approval_context.approval_request = Some(request.clone());

        let outcome = hook(approval_context).await.unwrap();

        assert!(outcome.block);
        assert_eq!(outcome.reason.as_deref(), Some("downstream denied"));
        assert!(outcome.approval_token.is_none());
        assert_eq!(
            request.issue().await.unwrap(),
            matrix_token(),
            "downstream denial must not consume the prepared request"
        );
    }

    #[tokio::test]
    async fn yolo_capability_allow_issues_final_token_when_request_present() {
        // Yolo never requires approval for a Read-capability tool, so this is
        // the capability-allow automatic path (not a path rule).
        let request = matrix_request();
        let hook = host_approval_before_tool_call(
            ApprovalMode::Yolo,
            ExtensionMode::Print,
            None,
            None,
            PathBuf::new(),
            empty_permission_rules(),
        );
        let mut context = context("read", ToolCapability::Read);
        context.approval_request = Some(request.clone());
        let outcome = hook(context).await.unwrap();
        assert!(!outcome.block);
        assert_eq!(
            outcome.approval_token.expect("yolo allow mints token"),
            matrix_token()
        );
        assert!(request.issue().await.is_err(), "minted exactly once");
    }

    #[tokio::test]
    async fn yolo_capability_allow_issues_no_token_without_request() {
        let hook = host_approval_before_tool_call(
            ApprovalMode::Yolo,
            ExtensionMode::Print,
            None,
            None,
            PathBuf::new(),
            empty_permission_rules(),
        );
        let outcome = hook(context("read", ToolCapability::Read)).await.unwrap();
        assert!(!outcome.block);
        assert!(outcome.approval_token.is_none());
    }

    #[tokio::test]
    async fn ask_confirmed_issues_final_token() {
        let adapter = ExtensionUiAdapter::new();
        let events = adapter.subscribe();
        let responder = tokio::spawn(answer_confirmation(
            events,
            adapter.clone(),
            HostResponse::Allow,
        ));
        let request = matrix_request();
        let hook = host_approval_before_tool_call(
            ApprovalMode::Ask,
            ExtensionMode::Tui,
            Some(adapter),
            None,
            PathBuf::new(),
            empty_permission_rules(),
        );
        let mut context = context("read", ToolCapability::Read);
        context.approval_request = Some(request.clone());
        let outcome = hook(context).await.unwrap();
        responder.await.unwrap();
        assert!(!outcome.block);
        assert_eq!(
            outcome.approval_token.expect("confirmed ask mints token"),
            matrix_token()
        );
        assert!(request.issue().await.is_err(), "minted exactly once");
    }

    #[tokio::test]
    async fn ask_denied_blocks_and_issues_no_token() {
        let adapter = ExtensionUiAdapter::new();
        let events = adapter.subscribe();
        let responder = tokio::spawn(answer_confirmation(
            events,
            adapter.clone(),
            HostResponse::Deny,
        ));
        let request = matrix_request();
        let hook = host_approval_before_tool_call(
            ApprovalMode::Ask,
            ExtensionMode::Tui,
            Some(adapter),
            None,
            PathBuf::new(),
            empty_permission_rules(),
        );
        let mut context = context("read", ToolCapability::Read);
        context.approval_request = Some(request.clone());
        let outcome = hook(context).await.unwrap();
        responder.await.unwrap();
        assert!(outcome.block, "denied ask must block");
        assert!(
            outcome.approval_token.is_none(),
            "denied ask must not mint a token"
        );
        // The request was never consumed: the sandbox stays fail-closed and
        // the token may still be minted later (e.g. by a re-issued call).
        assert_eq!(
            request.issue().await.unwrap(),
            matrix_token(),
            "denied ask must leave the approval request unissued"
        );
    }

    #[tokio::test]
    async fn ask_cancelled_blocks_and_issues_no_token() {
        let adapter = ExtensionUiAdapter::new();
        let events = adapter.subscribe();
        let responder = tokio::spawn(answer_confirmation(
            events,
            adapter.clone(),
            HostResponse::Cancel,
        ));
        let request = matrix_request();
        let hook = host_approval_before_tool_call(
            ApprovalMode::Ask,
            ExtensionMode::Tui,
            Some(adapter),
            None,
            PathBuf::new(),
            empty_permission_rules(),
        );
        let mut context = context("read", ToolCapability::Read);
        context.approval_request = Some(request.clone());
        let outcome = hook(context).await.unwrap();
        responder.await.unwrap();
        assert!(outcome.block, "cancelled ask must block");
        assert!(
            outcome.approval_token.is_none(),
            "cancelled ask mints no token"
        );
        assert_eq!(
            request.issue().await.unwrap(),
            matrix_token(),
            "cancelled ask must leave the approval request unissued"
        );
    }

    #[tokio::test]
    async fn ask_unavailable_blocks_and_issues_no_token() {
        // No interactive adapter: host approval is required but unavailable.
        // The call fails closed and the sandbox token is never minted.
        let request = matrix_request();
        let hook = host_approval_before_tool_call(
            ApprovalMode::Ask,
            ExtensionMode::Print,
            None,
            None,
            PathBuf::new(),
            empty_permission_rules(),
        );
        let mut context = context("read", ToolCapability::Read);
        context.approval_request = Some(request.clone());
        let outcome = hook(context).await.unwrap();
        assert!(outcome.block, "unavailable adapter must block");
        assert!(
            outcome
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("no interactive confirmation adapter")),
            "{:?}",
            outcome.reason
        );
        assert!(
            outcome.approval_token.is_none(),
            "unavailable mints no token"
        );
        assert_eq!(
            request.issue().await.unwrap(),
            matrix_token(),
            "unavailable must leave the approval request unissued"
        );
    }
}
