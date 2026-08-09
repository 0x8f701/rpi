//! Extension API conformance: dead registration categories must fail closed.
//!
//! The former Bun bridge conformance suite ran the same assertions against a
//! real Bun process; the in-process QuickJS runtime (tests/extensions.rs) now
//! owns the event/hook/UI/action/tool conformance semantics, so this file only
//! keeps the runtime-independent host-side registration validation, re-pointed
//! at the QuickJS .mjs fixtures.

use std::{collections::BTreeSet, path::PathBuf, time::Duration};

use anyhow::Result;
use pi_coding::{
    ExtensionCapability, ExtensionMode, ExtensionOrigin, ExtensionPermissionSet,
    ExtensionRuntime, ExtensionRuntimeOptions, ExtensionSpec, ExtensionSpecRuntime,
};

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/extensions");

fn options() -> ExtensionRuntimeOptions {
    ExtensionRuntimeOptions {
        mode: ExtensionMode::Tui,
        handshake_timeout: Duration::from_secs(10),
        load_timeout: Duration::from_secs(10),
        initialize_timeout: Duration::from_secs(10),
        invocation_timeout: Duration::from_secs(10),
        hook_timeout: Duration::from_secs(10),
        shutdown_timeout: Duration::from_secs(2),
        ..ExtensionRuntimeOptions::default()
    }
}

fn quickjs_spec(
    id: &str,
    fixture: &str,
    capabilities: impl IntoIterator<Item = ExtensionCapability>,
) -> ExtensionSpec {
    let entry = PathBuf::from(FIXTURES).join(fixture);
    ExtensionSpec::new_runtime(
        id,
        ExtensionSpecRuntime::QuickJs { entry },
        PathBuf::from(FIXTURES),
        ExtensionOrigin::Project,
        true,
        ExtensionPermissionSet {
            capabilities: capabilities.into_iter().collect::<BTreeSet<_>>(),
            ui_capabilities: BTreeSet::new(),
        },
    )
}

#[tokio::test]
async fn dead_registration_categories_fail_closed() -> Result<()> {
    let cases = [
        (
            "unsupported-tool-fields",
            "unsupported-tool-fields.mjs",
            vec![ExtensionCapability::Tools],
            "registerTool.constrainedSampling",
        ),
        (
            "invalid-tool-capability",
            "invalid-tool-capability.mjs",
            vec![ExtensionCapability::Tools],
            "registerTool capability must be read, write, or exec",
        ),
        (
            "renderer-registration",
            "renderer-registration.mjs",
            vec![ExtensionCapability::MessageRenderers],
            "renderer",
        ),
    ];
    for (id, fixture, capabilities, expected) in cases {
        let runtime = ExtensionRuntime::process(None, options());
        let report = runtime
            .load(vec![quickjs_spec(id, fixture, capabilities)])
            .await;
        assert_eq!(
            report.failures.len(),
            1,
            "{id} must not load as a dead registration"
        );
        let message = report.failures[0].message.to_ascii_lowercase();
        assert!(
            message.contains(&expected.to_ascii_lowercase()),
            "unexpected {id} failure: {}",
            report.failures[0].message
        );
        assert!(runtime.tools().is_empty());
        assert!(runtime.provider_metadata().is_empty());
        assert!(runtime.message_renderers().is_empty());
        runtime.shutdown().await;
    }
    Ok(())
}

#[tokio::test]
async fn provider_registration_loads_with_the_provider_capability() -> Result<()> {
    // registerProvider is a live registration category now: with the
    // `provider` capability granted the fixture loads and exposes the
    // provider descriptor.
    let runtime = ExtensionRuntime::process(None, options());
    let report = runtime
        .load(vec![quickjs_spec(
            "provider-registration",
            "provider-registration.mjs",
            vec![ExtensionCapability::Provider],
        )])
        .await;
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    let providers = runtime.providers();
    assert_eq!(providers.len(), 1, "the fixture registers exactly one provider");
    assert_eq!(providers[0].id, "provider-registration");
    assert_eq!(providers[0].api, "provider-registration-api");
    assert_eq!(providers[0].label.as_deref(), Some("Provider Registration"));
    assert_eq!(providers[0].capabilities, vec!["streaming".to_owned()]);
    runtime.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn provider_registration_requires_the_provider_capability() -> Result<()> {
    // The capability gate fails closed: the same fixture loaded without the
    // `provider` grant is rejected with an actionable message.
    let runtime = ExtensionRuntime::process(None, options());
    let report = runtime
        .load(vec![quickjs_spec(
            "provider-registration-ungranted",
            "provider-registration.mjs",
            vec![ExtensionCapability::Commands],
        )])
        .await;
    assert_eq!(report.failures.len(), 1, "{:?}", report.failures);
    let message = report.failures[0].message.to_ascii_lowercase();
    assert!(
        message.contains("provider"),
        "unexpected failure: {}",
        report.failures[0].message
    );
    assert!(runtime.providers().is_empty());
    runtime.shutdown().await;
    Ok(())
}

/// The shipped `examples/quickjs_extension.mjs` must load against the current
/// QuickJS API and its command/tool/hook registrations must be live — the
/// example is the documentation surface, so drift here is a real defect.
#[tokio::test]
async fn shipped_quickjs_example_loads_and_its_surface_is_live() -> Result<()> {
    use std::collections::BTreeSet;

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let examples_dir = manifest_dir.join("../../examples");
    let entry = examples_dir.join("quickjs_extension.mjs");
    assert!(entry.exists(), "example file must exist: {}", entry.display());

    let runtime = ExtensionRuntime::process(None, options());
    let report = runtime
        .load(vec![ExtensionSpec::new_runtime(
            "quickjs-example",
            ExtensionSpecRuntime::QuickJs { entry },
            examples_dir,
            ExtensionOrigin::Project,
            true,
            ExtensionPermissionSet {
                capabilities: BTreeSet::from([
                    ExtensionCapability::Commands,
                    ExtensionCapability::Tools,
                    ExtensionCapability::EventHooks,
                    ExtensionCapability::SessionActions,
                    ExtensionCapability::Ui,
                ]),
                ui_capabilities: BTreeSet::from([
                    pi_coding::ExtensionUiCapability::Notify,
                    pi_coding::ExtensionUiCapability::Status,
                ]),
            },
        )])
        .await;
    assert!(report.failures.is_empty(), "example must load: {:?}", report.failures);

    // Commands registered by the example.
    let command_names = runtime
        .commands()
        .into_iter()
        .map(|command| command.name)
        .collect::<BTreeSet<_>>();
    assert!(
        command_names.contains("quickjs-hello") && command_names.contains("quickjs-session"),
        "example commands must be registered: {command_names:?}"
    );

    // Tool registered by the example and executable end to end.
    let tools = runtime.tools();
    assert_eq!(tools.len(), 1, "example registers exactly one tool");
    assert_eq!(tools[0].name, "quickjs_echo");
    let (_, signal) = pi_agent::AbortController::new();
    let result: pi_agent::AgentToolResult = runtime
        .invoke_tool(
            "quickjs_echo",
            "call-example-1".to_owned(),
            serde_json::json!({ "text": "works" }),
            signal,
            None,
        )
        .await?;
    let text = result
        .content
        .into_iter()
        .map(|block| match block {
            pi_ai::ContentBlock::Text { text, .. } => text,
            _ => String::new(),
        })
        .collect::<String>();
    assert_eq!(text, "echo:works", "example tool must round-trip through QuickJS");

    runtime.shutdown().await;
    Ok(())
}
