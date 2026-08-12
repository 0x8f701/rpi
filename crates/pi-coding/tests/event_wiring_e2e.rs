//! End-to-end wiring tests for the extension events that were once
//! allow-listed with no production producer and are now wired.
//!
//! Each test loads a QuickJS extension that registers the event, drives the
//! real production entry point, and proves the extension received the event
//! with the exact payload. Because these are advisory observation events
//! (their hook outcomes are discarded by the session/agent event forwarders),
//! the fixture deliberately throws the received payload into the runtime's
//! `InvocationFailed` stream — the only positive receipt channel for
//! fire-and-forget emits — and the test parses it back out.
//!
//! Removed names (`project_trust`, `resources_discover`, `overlay_open`,
//! `overlay_close`) are covered by the rejection tests in
//! `tests/extensions.rs` (`removed_event_names_fail_registration`,
//! `quickjs_overlay_lifecycle_events_are_rejected_at_registration`).

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::future::Future;
use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use pi_ai::providers::{
    FauxProviderOptions, FauxResponse, register_faux_provider,
};
use pi_ai::Model;
use pi_coding::{
    Application, ApplicationEvent, ExtensionCapability, ExtensionMode, ExtensionOrigin,
    ExtensionPermissionSet, ExtensionRuntime, ExtensionRuntimeEvent, ExtensionRuntimeOptions,
    ExtensionSpec, ExtensionSpecRuntime, Session, SessionEvent, SessionOptions,
};
use serde_json::{Value, json};
use tempfile::TempDir;

/// Session model that does not claim any thinking-level support, so the
/// recorded level after `set_thinking_level` is whatever the session clamps
/// to — the event must carry that effective level.
fn lifecycle_model() -> Model {
    let suffix = uuid::Uuid::now_v7().to_string();
    Model {
        id: format!("lifecycle-model-{suffix}"),
        name: "Lifecycle Test Model".to_owned(),
        api: format!("lifecycle-api-{suffix}"),
        provider: format!("lifecycle-provider-{suffix}"),
        ..Model::default()
    }
}

/// Per-process isolated native session root so `start_new_recording()` never
/// writes into the real `~/.pi/agent/sessions` tree (Web sidebar source).
fn test_sessions_root() -> std::path::PathBuf {
    static ROOT: std::sync::LazyLock<tempfile::TempDir> = std::sync::LazyLock::new(|| {
        tempfile::tempdir().expect("test sessions root")
    });
    ROOT.path().to_path_buf()
}

fn session_with(model: Model) -> Session {
    let session = Session::new(SessionOptions {
        model,
        cwd: std::env::current_dir().expect("current directory"),
        system_prompt: String::new(),
        thinking_level: pi_agent::ThinkingLevel::Off,
        api_key: String::new(),
        compaction: None,
        stream_options: Default::default(),
        tools: Some(Vec::new()),
        before_tool_call: None,
        after_tool_call: None,
        stream_fn: None,
        auth_resolver: None,
    })
.expect("build session");
    session.set_session_dir(test_sessions_root());
    session
}

/// Write the extension entry and load it into a fresh in-process runtime.
async fn load_runtime(dir: &Path, id: &str, source: &str) -> Result<(ExtensionRuntime, ExtensionPermissionSet)> {
    let entry = dir.join(format!("{id}.mjs"));
    fs::write(&entry, source).context("writing extension entry")?;
    let permissions = ExtensionPermissionSet {
        capabilities: BTreeSet::from([ExtensionCapability::EventHooks]),
        ui_capabilities: BTreeSet::new(),
    };
    let spec = ExtensionSpec::new_runtime(
        id,
        ExtensionSpecRuntime::QuickJs { entry: entry.clone() },
        dir.to_path_buf(),
        ExtensionOrigin::Project,
        true,
        permissions.clone(),
    );
    let runtime = ExtensionRuntime::process(
        None,
        ExtensionRuntimeOptions {
            mode: ExtensionMode::Tui,
            hook_timeout: Duration::from_secs(10),
            ..ExtensionRuntimeOptions::default()
        },
    );
    let report = runtime.load(vec![spec]).await;
    anyhow::ensure!(report.failures.is_empty(), "{:?}", report.failures);
    Ok((runtime, permissions))
}

/// Build an Application bound to the loaded extension runtime.
async fn application_with_runtime(
    session: Session,
    runtime: ExtensionRuntime,
    permissions: ExtensionPermissionSet,
) -> Application {
    Application::new_with_extensions(session, runtime, permissions).await
}

/// The fixture throws `received:<json>` from the event handler; this drains
/// a runtime event stream subscribed BEFORE the production point is driven
/// (the `InvocationFailed` broadcast is not buffered without receivers) until
/// that `InvocationFailed` for `event:<name>` arrives and returns the
/// embedded JSON payload.
async fn received_event_payload(
    events: &mut tokio::sync::broadcast::Receiver<ExtensionRuntimeEvent>,
    event: &str,
) -> Result<Value> {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let received = tokio::time::timeout(Duration::from_millis(500), events.recv()).await;
        match received {
            Ok(Ok(ExtensionRuntimeEvent::InvocationFailed { operation, message, .. }))
                if operation == format!("event:{event}") =>
            {
                // The failure message is the JS error, possibly followed by
                // an `; child stderr: ...` suffix from the invocation bridge.
                let payload = message
                    .split_once("; child stderr")
                    .map(|(payload, _)| payload)
                    .unwrap_or(&message);
                let payload = payload
                    .rsplit_once("received:")
                    .map(|(_, payload)| payload)
                    .ok_or_else(|| anyhow!("unexpected failure message for {event}: {message}"))?;
                return serde_json::from_str(payload)
                    .with_context(|| format!("parsing received payload: {payload}"));
            }
            Ok(Ok(_)) => {}
            Ok(Err(_)) => return Err(anyhow!("runtime event stream closed")),
            Err(_) => {
                if std::time::Instant::now() > deadline {
                    return Err(anyhow!("extension never received event:{event}"));
                }
            }
        }
    }
}

/// Drive the production point and assert the extension received the payload
/// AND the matching ApplicationEvent was published (the extension emit
/// happens immediately before the ApplicationEvent publish in the same
/// forwarder task). The runtime event stream must have been subscribed
/// before the drive.
async fn assert_forwarded_session_event(
    application: &Application,
    runtime_events: &mut tokio::sync::broadcast::Receiver<ExtensionRuntimeEvent>,
    event_name: &str,
    drive: impl Future<Output = ()>,
    application_event: impl Fn(&SessionEvent) -> bool,
) -> Result<Value> {
    let mut app_events = application.subscribe();
    drive.await;
    // The extension event is emitted before the ApplicationEvent publish in
    // the same loop task, so awaiting the broadcast is race-free.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut saw_application_event = false;
    while std::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(500), app_events.recv()).await {
            Ok(Ok(ApplicationEvent::Session(event))) if application_event(&event) => {
                saw_application_event = true;
                break;
            }
            Ok(Ok(_)) => {}
            Ok(Err(_)) => break,
            Err(_) => continue,
        }
    }
    assert!(
        saw_application_event,
        "{event_name} must be published to the application event stream"
    );
    received_event_payload(runtime_events, event_name).await
}

#[tokio::test]
async fn session_info_changed_reaches_extension_on_rename() -> Result<()> {
    let dir = TempDir::new()?;
    let (runtime, permissions) = load_runtime(
        dir.path(),
        "session-info",
        r#"export default function (pi) {
  pi.on("session_info_changed", (event) => {
    const { signal, ...data } = event;
    throw new Error("received:" + JSON.stringify(data));
  });
}
"#,
    )
    .await?;
    let mut runtime_events = runtime.subscribe();
    let application = application_with_runtime(session_with(lifecycle_model()), runtime.clone(), permissions).await;

    let payload = assert_forwarded_session_event(
        &application,
        &mut runtime_events,
        "session_info_changed",
        async {
            application
                .set_session_name("Renamed Session")
                .expect("set name");
        },
        |event| matches!(event, SessionEvent::SessionInfoChanged { name } if name.as_deref() == Some("Renamed Session")),
    )
    .await?;
    assert_eq!(payload["name"], json!("Renamed Session"));

    application.cleanup().await;
    runtime.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn model_select_reaches_extension_on_model_change() -> Result<()> {
    let dir = TempDir::new()?;
    let (runtime, permissions) = load_runtime(
        dir.path(),
        "model-select",
        r#"export default function (pi) {
  pi.on("model_select", (event) => {
    const { signal, ...data } = event;
    throw new Error("received:" + JSON.stringify(data));
  });
}
"#,
    )
    .await?;
    let model = lifecycle_model();
    let mut runtime_events = runtime.subscribe();
    let application = application_with_runtime(session_with(model.clone()), runtime.clone(), permissions).await;

    let payload = assert_forwarded_session_event(
        &application,
        &mut runtime_events,
        "model_select",
        async {
            application.set_model(model.clone(), String::new());
        },
        |event| matches!(event, SessionEvent::ModelSelect { model: selected } if selected.id == model.id),
    )
    .await?;
    assert_eq!(payload["model"]["id"], json!(model.id));
    assert_eq!(payload["model"]["provider"], json!(model.provider));

    application.cleanup().await;
    runtime.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn model_select_extension_event_strips_credential_headers() -> Result<()> {
    let dir = TempDir::new()?;
    let (runtime, permissions) = load_runtime(
        dir.path(),
        "model-select-sanitized",
        r#"export default function (pi) {
  pi.on("model_select", (event) => {
    const { signal, ...data } = event;
    throw new Error("received:" + JSON.stringify(data));
  });
}
"#,
    )
    .await?;
    // Headers are assembled at runtime so no credential-shaped literal ever
    // appears in source; they must never cross the EventHooks boundary.
    let auth = format!("Bearer {}", uuid::Uuid::now_v7());
    let api_key = format!("api-key-{}", uuid::Uuid::now_v7());
    let suffix = uuid::Uuid::now_v7().to_string();
    let model = Model {
        id: format!("sanitized-model-{suffix}"),
        name: "Sanitized Model".to_owned(),
        api: format!("sanitized-api-{suffix}"),
        provider: format!("sanitized-provider-{suffix}"),
        headers: Some(HashMap::from([
            ("Authorization".to_owned(), auth.clone()),
            ("X-Api-Key".to_owned(), api_key.clone()),
        ])),
        ..Model::default()
    };
    let mut runtime_events = runtime.subscribe();
    let application = application_with_runtime(session_with(model.clone()), runtime.clone(), permissions).await;

    // Ordinary in-process model selection is unchanged: the ApplicationEvent
    // still carries the credential-bearing model.
    let payload = assert_forwarded_session_event(
        &application,
        &mut runtime_events,
        "model_select",
        async {
            application.set_model(model.clone(), String::new());
        },
        |event| matches!(event, SessionEvent::ModelSelect { model: selected } if selected.id == model.id && selected.headers == model.headers),
    )
    .await?;

    // The extension receives the public projection only: id/provider are
    // present, and neither the header names, the header values, nor a
    // headers object appear anywhere in the emitted JSON.
    assert_eq!(payload["model"]["id"], json!(model.id));
    assert_eq!(payload["model"]["provider"], json!(model.provider));
    assert!(
        payload["model"]["headers"].is_null(),
        "no headers object may reach the extension: {payload}"
    );
    let encoded = payload.to_string();
    assert!(
        !encoded.contains(auth.as_str()),
        "authorization value must not reach the extension"
    );
    assert!(
        !encoded.contains(api_key.as_str()),
        "api key value must not reach the extension"
    );
    assert!(
        !encoded.contains("Authorization"),
        "authorization header name must not reach the extension"
    );
    assert!(
        !encoded.contains("X-Api-Key"),
        "api key header name must not reach the extension"
    );

    application.cleanup().await;
    runtime.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn thinking_level_select_reaches_extension_on_level_change() -> Result<()> {
    let dir = TempDir::new()?;
    let (runtime, permissions) = load_runtime(
        dir.path(),
        "thinking-level-select",
        r#"export default function (pi) {
  pi.on("thinking_level_select", (event) => {
    const { signal, ...data } = event;
    throw new Error("received:" + JSON.stringify(data));
  });
}
"#,
    )
    .await?;
    let mut runtime_events = runtime.subscribe();
    let application = application_with_runtime(session_with(lifecycle_model()), runtime.clone(), permissions).await;

    let payload = assert_forwarded_session_event(
        &application,
        &mut runtime_events,
        "thinking_level_select",
        async {
            application.set_thinking_level(pi_agent::ThinkingLevel::High);
        },
        |event| matches!(event, SessionEvent::ThinkingLevelSelect { .. }),
    )
    .await?;
    // The payload carries the effective level (post-clamp), which must match
    // the session state the application reports.
    let effective = application.state().await.thinking_level;
    assert_eq!(payload["thinkingLevel"], json!(effective));

    application.cleanup().await;
    runtime.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn session_tree_reaches_extension_after_navigation() -> Result<()> {
    let dir = TempDir::new()?;
    let (runtime, permissions) = load_runtime(
        dir.path(),
        "session-tree",
        r#"export default function (pi) {
  pi.on("session_tree", (event) => {
    const { signal, ...data } = event;
    throw new Error("received:" + JSON.stringify(data));
  });
}
"#,
    )
    .await?;

    // A recorded session with one turn so navigate_tree has an entry to jump
    // to (mirrors tests/application_runtime.rs navigation tests).
    let suffix = uuid::Uuid::now_v7().to_string();
    let api = format!("session-tree-api-{suffix}");
    let provider = format!("session-tree-provider-{suffix}");
    let model = Model {
        id: "session-tree-model".to_owned(),
        name: "Session Tree Model".to_owned(),
        api: api.clone(),
        provider: provider.clone(),
        ..Model::default()
    };
    let registration = register_faux_provider(FauxProviderOptions {
        api,
        provider,
        models: vec![model.clone()],
        chunk_size: 1,
    });
    registration.set_responses(vec![FauxResponse::text("answer")]);
    let session = session_with(model);
    session.start_new_recording().expect("start recorder");
    session
        .run("question", Vec::new())
        .await
        .expect("record turn");

    let mut runtime_events = runtime.subscribe();
    let application = application_with_runtime(session, runtime.clone(), permissions).await;
    let user_id = application
        .session_entries(None)
        .expect("entries")
        .entries
        .into_iter()
        .find(|entry| matches!(entry.message, Some(pi_ai::Message::User(_))))
        .expect("user entry")
        .id;

    let mut app_events = application.subscribe();
    let result = application
        .navigate_tree(&user_id, pi_coding::NavigateTreeOptions::default())
        .await
        .expect("navigate");
    // The production point: SessionTree is published to the app stream and
    // the extension emit happens on the same navigation.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut saw_session_tree = false;
    while std::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(500), app_events.recv()).await {
            Ok(Ok(ApplicationEvent::SessionTree(event))) => {
                assert_eq!(event.target_id, user_id);
                saw_session_tree = true;
                break;
            }
            Ok(Ok(_)) => {}
            Ok(Err(_)) => break,
            Err(_) => continue,
        }
    }
    assert!(saw_session_tree, "session_tree must be published after navigation");

    let payload = received_event_payload(&mut runtime_events, "session_tree").await?;
    assert_eq!(payload["targetId"], json!(user_id));
    assert_eq!(payload["editorText"], json!("question"));
    assert_eq!(payload["changed"], json!(result.changed));
    assert_eq!(payload["cancelled"], json!(false));
    assert_eq!(
        payload["activeLeafId"],
        serde_json::to_value(result.active_leaf_id)?,
    );
    assert_eq!(
        payload["summaryEntryId"],
        serde_json::to_value(result.summary_entry_id)?,
    );

    application.cleanup().await;
    runtime.shutdown().await;
    registration.unregister();
    Ok(())
}

#[tokio::test]
async fn agent_settled_reaches_extension_after_turn() -> Result<()> {
    let dir = TempDir::new()?;
    let (runtime, permissions) = load_runtime(
        dir.path(),
        "agent-settled",
        r#"export default function (pi) {
  pi.on("agent_settled", (event) => {
    const { signal, ...data } = event;
    throw new Error("received:" + JSON.stringify(data));
  });
}
"#,
    )
    .await?;

    let suffix = uuid::Uuid::now_v7().to_string();
    let api = format!("agent-settled-api-{suffix}");
    let provider = format!("agent-settled-provider-{suffix}");
    let model = Model {
        id: "agent-settled-model".to_owned(),
        name: "Agent Settled Model".to_owned(),
        api: api.clone(),
        provider: provider.clone(),
        ..Model::default()
    };
    let registration = register_faux_provider(FauxProviderOptions {
        api,
        provider,
        models: vec![model.clone()],
        chunk_size: 1,
    });
    registration.set_responses(vec![FauxResponse::text("hello")]);
    let mut runtime_events = runtime.subscribe();
    let application = application_with_runtime(session_with(model), runtime.clone(), permissions).await;

    let mut app_events = application.subscribe();
    application
        .prompt("say hello".to_owned(), Vec::new(), None)
        .await
        .expect("accept prompt");
    // The settle point: AgentSettled is published after the parent turn
    // finishes (and the extension emit is spawned on that same point).
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut saw_settled = false;
    while std::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(500), app_events.recv()).await {
            Ok(Ok(ApplicationEvent::AgentSettled)) => {
                saw_settled = true;
                break;
            }
            Ok(Ok(_)) => {}
            Ok(Err(_)) => break,
            Err(_) => continue,
        }
    }
    assert!(saw_settled, "AgentSettled must be published after the turn");

    let payload = received_event_payload(&mut runtime_events, "agent_settled").await?;
    // The host injects `type` into the event object; the event carries no
    // other state.
    assert_eq!(payload, json!({ "type": "agent_settled" }), "agent_settled carries no state");

    application.cleanup().await;
    runtime.shutdown().await;
    registration.unregister();
    Ok(())
}
