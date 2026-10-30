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
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use pi_ai::Model;
use pi_ai::providers::{
    FauxProviderOptions, FauxProviderRegistration, FauxResponse, register_faux_provider,
};
use pi_coding::{
    Application, ApplicationEvent, ApplicationRuntimeCandidate, ExtensionCapability, ExtensionMode,
    ExtensionPermissionSet, ExtensionRuntime, ExtensionRuntimeEvent, ExtensionRuntimeOptions,
    ResourceManager, ResourceManagerOptions, Session, SessionEvent, SessionOptions,
};
use serde_json::{Value, json};
use tempfile::TempDir;

/// Session model that does not claim any thinking-level support, so the
/// recorded level after `set_thinking_level` is whatever the session clamps
/// to — the event must carry that effective level. The registration makes the
/// model resolvable during Application HTTP-authority staging without network.
fn lifecycle_model() -> (Model, FauxProviderRegistration) {
    faux_model("lifecycle", Vec::new())
}

fn faux_model(prefix: &str, responses: Vec<FauxResponse>) -> (Model, FauxProviderRegistration) {
    let suffix = uuid::Uuid::now_v7().to_string();
    let model = Model {
        id: format!("{prefix}-model-{suffix}"),
        name: format!("{prefix} Test Model"),
        api: format!("{prefix}-api-{suffix}"),
        provider: format!("{prefix}-provider-{suffix}"),
        base_url: "http://localhost:0".to_owned(),
        ..Model::default()
    };
    let registration = register_faux_provider(FauxProviderOptions {
        api: model.api.clone(),
        provider: model.provider.clone(),
        models: vec![model.clone()],
        chunk_size: 1,
    });
    registration.set_responses(responses);
    (model, registration)
}

fn workspace_root(root: &Path) -> PathBuf {
    root.join("workspace")
}

fn session_root(root: &Path) -> PathBuf {
    root.join("sessions")
}

fn session_with(model: Model, root: &Path) -> Session {
    let workspace = workspace_root(root);
    let sessions = session_root(root);
    fs::create_dir_all(&workspace).expect("test workspace");
    fs::create_dir_all(&sessions).expect("test sessions root");
    let session = Session::new(SessionOptions {
        model,
        cwd: workspace,
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
        auth_manager: None,
    })
    .expect("build session");
    session.set_session_dir(sessions);
    session
}

/// Write the extension package and stage it through the application's explicit-resource path.
async fn application_with_runtime(
    session: Session,
    dir: &Path,
    id: &str,
    source: &str,
) -> Result<(Application, ExtensionRuntime)> {
    let entry = dir.join(format!("{id}.mjs"));
    fs::write(&entry, source).context("writing extension entry")?;
    let manifest = dir.join("pi-extension.json");
    fs::write(
        &manifest,
        serde_json::to_vec_pretty(&json!({
            "schemaVersion": 1,
            "id": id,
            "runtime": "quickjs",
            "entry": entry.file_name().expect("extension entry file name").to_string_lossy(),
            "capabilities": ["event_hooks"],
        }))?,
    )?;
    let permissions = ExtensionPermissionSet {
        capabilities: BTreeSet::from([ExtensionCapability::EventHooks]),
        ui_capabilities: BTreeSet::new(),
    };
    let runtime = ExtensionRuntime::process(
        None,
        ExtensionRuntimeOptions {
            mode: ExtensionMode::Tui,
            hook_timeout: Duration::from_secs(10),
            ..ExtensionRuntimeOptions::default()
        },
    );
    let agent_dir = dir.join("agent");
    fs::create_dir_all(&agent_dir).context("creating test agent directory")?;
    fs::write(
        agent_dir.join("settings.json"),
        r#"{"sandbox":{"mode":"danger-full-access"}}"#,
    )
    .context("writing test agent settings")?;
    let mut options = ResourceManagerOptions::new(session.cwd());
    options.agent_dir = agent_dir;
    options.project_trust_override = Some(true);
    options.disable_skills = true;
    options.disable_prompt_templates = true;
    options.disable_themes = true;
    options.disable_context_files = true;
    options.explicit_extension_paths = vec![manifest];
    session
        .attach_resources(ResourceManager::new(options)?)
        .await?;
    let application = Application::from_runtime_candidate(
        ApplicationRuntimeCandidate::new(session).with_extensions(runtime.clone(), permissions),
    )
    .await?;
    Ok((application, runtime))
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
            Ok(Ok(ExtensionRuntimeEvent::InvocationFailed {
                operation, message, ..
            })) if operation == format!("event:{event}") => {
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
    let (model, _registration) = lifecycle_model();
    let (application, runtime) = application_with_runtime(
        session_with(model, dir.path()),
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
    let (model, _registration) = lifecycle_model();
    let (application, runtime) = application_with_runtime(
        session_with(model.clone(), dir.path()),
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
    let mut runtime_events = runtime.subscribe();

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
    // Headers are assembled at runtime so no credential-shaped literal ever
    // appears in source; they must never cross the EventHooks boundary.
    let auth = format!("Bearer {}", uuid::Uuid::now_v7());
    let api_key = format!("api-key-{}", uuid::Uuid::now_v7());
    let (mut model, _registration) = faux_model("sanitized", Vec::new());
    model.headers = Some(HashMap::from([
        ("Authorization".to_owned(), auth.clone()),
        ("X-Api-Key".to_owned(), api_key.clone()),
    ]));
    let (application, runtime) = application_with_runtime(
        session_with(model.clone(), dir.path()),
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
    let mut runtime_events = runtime.subscribe();

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
    let (model, _registration) = lifecycle_model();
    let (application, runtime) = application_with_runtime(
        session_with(model, dir.path()),
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
    // A recorded session with one turn so navigate_tree has an entry to jump
    // to (mirrors tests/application_runtime.rs navigation tests).
    let (model, registration) = faux_model("session-tree", vec![FauxResponse::text("answer")]);
    let session = session_with(model, dir.path());
    session.start_new_recording().expect("start recorder");
    session
        .run("question", Vec::new())
        .await
        .expect("record turn");

    let (application, runtime) = application_with_runtime(
        session,
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
    let mut runtime_events = runtime.subscribe();
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
    assert!(
        saw_session_tree,
        "session_tree must be published after navigation"
    );

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
    let (model, registration) = faux_model("agent-settled", vec![FauxResponse::text("hello")]);
    let (application, runtime) = application_with_runtime(
        session_with(model, dir.path()),
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
    let mut runtime_events = runtime.subscribe();

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
    assert_eq!(
        payload,
        json!({ "type": "agent_settled" }),
        "agent_settled carries no state"
    );

    application.cleanup().await;
    runtime.shutdown().await;
    registration.unregister();
    Ok(())
}
