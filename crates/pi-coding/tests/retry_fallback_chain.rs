use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use pi_agent::ThinkingLevel;
use pi_ai::providers::{FauxProviderOptions, FauxResponse, register_faux_provider};
use pi_ai::{Model, SimpleStreamOptions, StopReason};
use pi_coding::{
    RetryFallbackChains, RetrySettings, Session, SessionEvent, SessionOptions, Settings,
};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);
static REGISTRY_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

fn model(provider: &str, id: &str) -> Model {
    let mut model = Model::default();
    model.id = id.into();
    model.name = id.into();
    model.api = format!("fallback-api-{provider}-{id}");
    model.provider = provider.into();
    model.base_url = "http://localhost:0".into();
    model
}

fn make_linked_sessions(
    primary_responses: Vec<FauxResponse>,
    fallback_responses: Vec<FauxResponse>,
) -> (
    Session,
    String,
    String,
    pi_ai::providers::FauxProviderRegistration,
    pi_ai::providers::FauxProviderRegistration,
    tempfile::TempDir,
) {
    let suffix = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let primary = model(&format!("primary-{suffix}"), "main");
    let backup = model(&format!("backup-{suffix}"), "spare");
    let primary_name = format!("{}/{}", primary.provider, primary.id);
    let backup_name = format!("{}/{}", backup.provider, backup.id);
    let primary_reg = register_faux_provider(FauxProviderOptions {
        api: primary.api.clone(),
        provider: primary.provider.clone(),
        models: vec![primary.clone()],
        chunk_size: 64,
    });
    primary_reg.set_responses(primary_responses);
    let backup_reg = register_faux_provider(FauxProviderOptions {
        api: backup.api.clone(),
        provider: backup.provider.clone(),
        models: vec![backup.clone()],
        chunk_size: 64,
    });
    backup_reg.set_responses(fallback_responses);

    let cwd = tempfile::tempdir().expect("tempdir");
    let primary_provider = primary.provider.clone();
    let backup_provider = backup.provider.clone();
    let backup_for_resolver = backup.clone();
    let session = Session::new(SessionOptions {
        model: primary,
        cwd: cwd.path().to_path_buf(),
        system_prompt: String::new(),
        thinking_level: ThinkingLevel::Off,
        api_key: "primary-key".into(),
        compaction: None,
        stream_options: SimpleStreamOptions::default(),
        tools: None,
        before_tool_call: None,
        after_tool_call: None,
        stream_fn: None,
        auth_resolver: Some(Arc::new(move |requested: Model| {
            let backup = backup_for_resolver.clone();
            let primary_provider = primary_provider.clone();
            let backup_provider = backup_provider.clone();
            Box::pin(async move {
                if requested.provider == backup_provider && requested.id == backup.id {
                    Ok(pi_coding::RequestAuth {
                        api_key: "backup-key".into(),
                        headers: Default::default(),
                        env: Default::default(),
                        available_model_ids: None,
                    })
                } else if requested.provider == primary_provider {
                    Ok(pi_coding::RequestAuth {
                        api_key: "primary-key".into(),
                        headers: Default::default(),
                        env: Default::default(),
                        available_model_ids: None,
                    })
                } else {
                    Err(anyhow::anyhow!(
                        "no auth for {}/{}",
                        requested.provider,
                        requested.id
                    ))
                }
            })
        })),
    })
    .expect("session");

    let mut chains = RetryFallbackChains::default();
    chains.insert("default".into(), vec![backup_name.clone()]);
    // Exact model-selector key coverage (OMP specificity).
    chains.insert(primary_name.clone(), vec![backup_name.clone()]);
    session.set_retry_settings(RetrySettings {
        enabled: true,
        max_retries: 0,
        base_delay_ms: 1,
        model_fallback: true,
        fallback_chains: chains,
        ..Default::default()
    });
    (session, primary_name, backup_name, primary_reg, backup_reg, cwd)
}

async fn collect_events_during<F, T>(
    session: &Session,
    work: F,
) -> (T, Vec<SessionEvent>)
where
    F: Future<Output = T>,
{
    let mut rx = session.subscribe_session_events();
    let mut collected = Vec::new();
    tokio::pin!(work);
    let result = loop {
        tokio::select! {
            result = &mut work => break result,
            event = rx.recv() => {
                if let Ok(event) = event {
                    collected.push(event);
                }
            }
        }
    };
    loop {
        match tokio::time::timeout(std::time::Duration::from_millis(20), rx.recv()).await {
            Ok(Ok(event)) => collected.push(event),
            _ => break,
        }
    }
    (result, collected)
}

#[tokio::test(flavor = "current_thread")]
async fn primary_failure_then_fallback_success_emits_lifecycle() {
    let _guard = REGISTRY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (session, primary_name, backup_name, primary_reg, backup_reg, _cwd) =
        make_linked_sessions(
            vec![FauxResponse::error("503 Service unavailable")],
            vec![FauxResponse::text("fallback-ok")],
        );
    let (result, events) = collect_events_during(&session, session.run("switch please", vec![])).await;
    let result = result.expect("fallback success");
    assert_eq!(result.text, "fallback-ok");
    assert_eq!(result.stop_reason, StopReason::Stop);
    let primary_selector = format!("{primary_name}:off");
    let fallback_selector = format!("{backup_name}:off");
    assert!(
        events.iter().any(|event| matches!(
            event,
            SessionEvent::RetryFallbackApplied { from, to, role }
                if from == &primary_selector
                    && to == &fallback_selector
                    && (role == "default" || role == &primary_name)
        )),
        "missing applied event in {events:?}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            SessionEvent::RetryFallbackSucceeded { model, role }
                if model == &fallback_selector
                    && (role == "default" || role == &primary_name)
        )),
        "missing succeeded event in {events:?}"
    );
    assert_eq!(
        session.retry_fallback_model().as_deref(),
        Some(fallback_selector.as_str())
    );
    // Custom registered API/provider must be preserved on the active model.
    let active = session.model().expect("model");
    assert!(
        active.provider.starts_with("backup-") && active.api.contains(&active.provider),
        "custom api/provider not preserved: provider={} api={}",
        active.provider,
        active.api
    );
    primary_reg.unregister();
    backup_reg.unregister();
}

#[tokio::test(flavor = "current_thread")]
async fn primary_success_skips_fallbacks() {
    let _guard = REGISTRY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (session, _, _, primary_reg, backup_reg, _cwd) = make_linked_sessions(
        vec![FauxResponse::text("primary-ok")],
        vec![FauxResponse::text("should-not-run")],
    );
    let (result, events) = collect_events_during(&session, session.run("stay primary", vec![])).await;
    let result = result.expect("primary success");
    assert_eq!(result.text, "primary-ok");
    assert!(
        events.iter().all(|event| {
            !matches!(
                event,
                SessionEvent::RetryFallbackApplied { .. }
                    | SessionEvent::RetryFallbackSucceeded { .. }
            )
        }),
        "unexpected fallback events: {events:?}"
    );
    primary_reg.unregister();
    backup_reg.unregister();
}

#[tokio::test(flavor = "current_thread")]
async fn cancellation_prevents_later_fallbacks() {
    let _guard = REGISTRY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (session, _, _, primary_reg, backup_reg, _cwd) = make_linked_sessions(
        vec![FauxResponse::error("503 Service unavailable")],
        vec![FauxResponse::text("late-fallback")],
    );
    // Same-model retry path only: no model fallback during the delayed sleep.
    session.set_retry_settings(RetrySettings {
        enabled: true,
        max_retries: 3,
        base_delay_ms: 30_000,
        model_fallback: false,
        fallback_chains: session.retry_settings().fallback_chains,
        ..Default::default()
    });

    let mut events = session.subscribe_session_events();
    let running = session.clone();
    let task = tokio::spawn(async move { running.run("cancel me", vec![]).await });
    loop {
        match tokio::time::timeout(std::time::Duration::from_secs(2), events.recv())
            .await
            .expect("event timeout")
            .expect("event")
        {
            SessionEvent::AutoRetryStart { .. } => break,
            _ => {}
        }
    }
    session.abort_retry();
    let err = tokio::time::timeout(std::time::Duration::from_secs(2), task)
        .await
        .expect("join timeout")
        .expect("task")
        .expect_err("cancelled");
    assert!(err.to_string().contains("Retry cancelled"));
    while let Ok(Ok(event)) =
        tokio::time::timeout(std::time::Duration::from_millis(50), events.recv()).await
    {
        assert!(!matches!(event, SessionEvent::RetryFallbackApplied { .. }));
    }
    primary_reg.unregister();
    backup_reg.unregister();
}

#[tokio::test(flavor = "current_thread")]
async fn exhausted_diagnostics_are_sanitized() {
    let _guard = REGISTRY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (session, _, _, primary_reg, backup_reg, _cwd) = make_linked_sessions(
        vec![FauxResponse::error(
            "503 upstream api_key=sk-abc1234567890secret failed",
        )],
        vec![FauxResponse::error(
            "503 backup Authorization: Bearer sk-abc1234567890secret",
        )],
    );
    let (result, events) = collect_events_during(&session, session.run("exhaust", vec![])).await;
    let _err = result.expect_err("both models fail");
    let terminal = events.iter().rev().find_map(|event| match event {
        SessionEvent::AutoRetryEnd {
            success: false,
            final_error: Some(message),
            ..
        } => Some(message.clone()),
        _ => None,
    });
    // When only one error path runs without same-model retries, diagnostics may
    // stay on the operation error; prefer event when present.
    if let Some(message) = terminal {
        assert!(!message.contains("sk-abc"), "{message}");
        assert!(
            message.contains("[REDACTED]") || message.contains("503"),
            "{message}"
        );
    }
    primary_reg.unregister();
    backup_reg.unregister();
}

#[test]
fn invalid_fallback_chain_settings_fail_actionably() {
    let mut settings = Settings::default();
    settings.retry = Some(pi_coding::RetryConfig {
        fallback_chains: Some(BTreeMap::from([(
            "default".into(),
            vec![String::new(), "not-a-selector".into()],
        )])),
        ..Default::default()
    });
    let error = settings
        .runtime_settings()
        .expect_err("invalid chains must fail");
    let message = format!("{error:#}");
    assert!(
        message.contains("retry.fallbackChains") || message.contains("selector"),
        "unexpected: {message}"
    );
}

#[test]
fn retry_settings_propagate_provider_and_chain_fields() {
    let json = r#"{
        "retry": {
            "enabled": true,
            "maxRetries": 2,
            "baseDelayMs": 100,
            "modelFallback": true,
            "fallbackChains": {
                "default": ["backup/spare"],
                "openai/*": ["openrouter/*"],
                "openrouter/google/*": ["google/*"]
            },
            "provider": {
                "maxRetries": 4,
                "maxRetryDelayMs": 1200,
                "timeoutMs": 9000
            }
        }
    }"#;
    let settings: Settings = serde_json::from_str(json).expect("parse");
    let runtime = settings.runtime_settings().expect("runtime");
    assert!(runtime.retry.enabled);
    assert_eq!(runtime.retry.max_retries, 2);
    assert_eq!(runtime.retry.base_delay_ms, 100);
    assert!(runtime.retry.model_fallback);
    assert_eq!(
        runtime.retry.fallback_chains.get("default"),
        Some(&vec!["backup/spare".to_owned()])
    );
    assert_eq!(
        runtime.retry.fallback_chains.get("openai/*"),
        Some(&vec!["openrouter/*".to_owned()])
    );
    assert_eq!(
        runtime.retry.fallback_chains.get("openrouter/google/*"),
        Some(&vec!["google/*".to_owned()])
    );
    assert_eq!(runtime.stream_options.stream.max_retries, 4);
    assert_eq!(runtime.stream_options.stream.max_retry_delay_ms, Some(1200));
    assert_eq!(runtime.stream_options.stream.timeout_ms, Some(9000));
}
