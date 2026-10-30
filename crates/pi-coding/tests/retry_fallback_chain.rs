use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use pi_agent::ThinkingLevel;
use pi_ai::providers::{FauxProviderOptions, FauxResponse, register_faux_provider};
use pi_ai::{Model, SimpleStreamOptions, StopReason};
use pi_coding::{
    AgentCatalog, AgentDefinition, AgentDefinitionSource, ChildSessionRequest, JobStatus,
    OrchestrationConfig, OrchestrationRuntime, RetryFallbackChains, RetrySettings, Session,
    SessionEvent, SessionOptions, Settings, TaskItem, YieldState,
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
                        kind: pi_coding::RequestAuthKind::ApiKeyOrHeaders,
                    })
                } else if requested.provider == primary_provider {
                    Ok(pi_coding::RequestAuth {
                        api_key: "primary-key".into(),
                        headers: Default::default(),
                        env: Default::default(),
                        available_model_ids: None,
                        kind: pi_coding::RequestAuthKind::ApiKeyOrHeaders,
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
        auth_manager: None,
    })
    .expect("session");

    let mut chains = RetryFallbackChains::default();
    chains.insert("default".into(), vec![backup_name.clone()]);
    chains.insert(primary_name.clone(), vec![backup_name.clone()]);
    session.set_retry_settings(RetrySettings {
        enabled: true,
        max_retries: 0,
        base_delay_ms: 1,
        model_fallback: true,
        fallback_chains: chains,
        ..Default::default()
    });
    (
        session,
        primary_name,
        backup_name,
        primary_reg,
        backup_reg,
        cwd,
    )
}

async fn collect_events_during<F, T>(session: &Session, work: F) -> (T, Vec<SessionEvent>)
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
    while let Ok(event) = rx.try_recv() {
        collected.push(event);
    }
    (result, collected)
}

#[tokio::test(flavor = "current_thread")]
async fn primary_failure_then_fallback_success_emits_lifecycle() {
    let _guard = REGISTRY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (session, primary_name, backup_name, primary_reg, backup_reg, _cwd) = make_linked_sessions(
        vec![FauxResponse::error("503 Service unavailable")],
        vec![FauxResponse::text("fallback-ok")],
    );
    let (result, events) =
        collect_events_during(&session, session.run("switch please", vec![])).await;
    let result = result.expect("fallback success");
    assert_eq!(result.text, "fallback-ok");
    assert_eq!(result.stop_reason, StopReason::Stop);
    assert!(
        events
            .iter()
            .all(|event| !matches!(event, SessionEvent::AutoRetryStart { .. })),
        "maxRetries=0 must not schedule a same-model retry: {events:?}"
    );
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
        events
            .iter()
            .filter(|event| matches!(event, SessionEvent::RetryFallbackApplied { .. }))
            .count(),
        1,
        "the exhausted primary must advance exactly once: {events:?}"
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
async fn transient_primary_succeeds_on_same_model_retry_without_fallback() {
    let _guard = REGISTRY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (session, primary_name, _, primary_reg, backup_reg, _cwd) = make_linked_sessions(
        vec![
            FauxResponse::error("503 transient primary failure"),
            FauxResponse::text("primary-retry-ok"),
        ],
        vec![FauxResponse::text("must-not-fallback")],
    );
    session.set_retry_settings(RetrySettings {
        enabled: true,
        max_retries: 1,
        base_delay_ms: 1,
        model_fallback: true,
        fallback_chains: session.retry_settings().fallback_chains,
        ..Default::default()
    });

    let (result, events) =
        collect_events_during(&session, session.run("retry primary", vec![])).await;
    let result = result.expect("same-model retry succeeds");
    assert_eq!(result.text, "primary-retry-ok");
    let starts = events
        .iter()
        .filter_map(|event| match event {
            SessionEvent::AutoRetryStart {
                attempt,
                max_attempts,
                ..
            } => Some((*attempt, *max_attempts)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(starts, vec![(1, 1)], "unexpected retry starts: {events:?}");
    assert!(
        events.iter().all(|event| {
            !matches!(
                event,
                SessionEvent::RetryFallbackApplied { .. }
                    | SessionEvent::RetryFallbackSucceeded { .. }
            )
        }),
        "transient primary failure must not apply fallback: {events:?}"
    );
    assert_eq!(
        session.model().expect("primary remains active").provider,
        primary_name.split('/').next().expect("primary provider")
    );
    assert!(session.retry_fallback_model().is_none());
    primary_reg.unregister();
    backup_reg.unregister();
}

#[tokio::test(flavor = "current_thread")]
async fn primary_exhausts_retry_budget_before_one_fallback_transition() {
    let _guard = REGISTRY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (session, primary_name, backup_name, primary_reg, backup_reg, _cwd) = make_linked_sessions(
        vec![
            FauxResponse::error("503 primary attempt zero"),
            FauxResponse::error("503 primary attempt one"),
            FauxResponse::error("503 primary attempt two"),
        ],
        vec![FauxResponse::text("fallback-after-primary-budget")],
    );
    session.set_retry_settings(RetrySettings {
        enabled: true,
        max_retries: 2,
        base_delay_ms: 1,
        model_fallback: true,
        fallback_chains: session.retry_settings().fallback_chains,
        ..Default::default()
    });

    let (result, events) =
        collect_events_during(&session, session.run("exhaust primary", vec![])).await;
    assert_eq!(
        result
            .expect("fallback succeeds after primary retries")
            .text,
        "fallback-after-primary-budget"
    );
    let recovery = events
        .iter()
        .filter(|event| {
            matches!(
                event,
                SessionEvent::AutoRetryStart { .. } | SessionEvent::RetryFallbackApplied { .. }
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(recovery.len(), 3, "unexpected recovery events: {events:?}");
    assert!(
        matches!(
            recovery[0],
            SessionEvent::AutoRetryStart {
                attempt: 1,
                max_attempts: 2,
                ..
            }
        ),
        "first primary retry missing: {events:?}"
    );
    assert!(
        matches!(
            recovery[1],
            SessionEvent::AutoRetryStart {
                attempt: 2,
                max_attempts: 2,
                ..
            }
        ),
        "second primary retry missing: {events:?}"
    );
    let primary_selector = format!("{primary_name}:off");
    let fallback_selector = format!("{backup_name}:off");
    assert!(
        matches!(
            recovery[2],
            SessionEvent::RetryFallbackApplied { from, to, .. }
                if from == &primary_selector && to == &fallback_selector
        ),
        "fallback must follow both primary retries: {events:?}"
    );
    primary_reg.unregister();
    backup_reg.unregister();
}

#[tokio::test(flavor = "current_thread")]
async fn fallback_candidate_receives_full_retry_budget_before_terminal_failure() {
    let _guard = REGISTRY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (session, _, _, primary_reg, backup_reg, _cwd) = make_linked_sessions(
        vec![
            FauxResponse::error("503 primary attempt zero"),
            FauxResponse::error("503 primary attempt one"),
            FauxResponse::error("503 primary attempt two"),
        ],
        vec![
            FauxResponse::error("503 fallback attempt zero"),
            FauxResponse::error("503 fallback attempt one"),
            FauxResponse::error("503 fallback attempt two"),
        ],
    );
    session.set_retry_settings(RetrySettings {
        enabled: true,
        max_retries: 2,
        base_delay_ms: 1,
        model_fallback: true,
        fallback_chains: session.retry_settings().fallback_chains,
        ..Default::default()
    });

    let (result, events) =
        collect_events_during(&session, session.run("exhaust chain", vec![])).await;
    let _error = result.expect_err("fallback exhausts its independent budget");
    let recovery = events
        .iter()
        .filter(|event| {
            matches!(
                event,
                SessionEvent::AutoRetryStart { .. } | SessionEvent::RetryFallbackApplied { .. }
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(recovery.len(), 5, "unexpected recovery events: {events:?}");
    for (index, expected_attempt) in [(0, 1), (1, 2), (3, 1), (4, 2)] {
        assert!(
            matches!(
                recovery[index],
                SessionEvent::AutoRetryStart {
                    attempt,
                    max_attempts: 2,
                    ..
                } if *attempt == expected_attempt
            ),
            "retry budget did not reset per model: {events:?}"
        );
    }
    assert!(
        matches!(recovery[2], SessionEvent::RetryFallbackApplied { .. }),
        "one fallback transition must separate the two retry budgets: {events:?}"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, SessionEvent::RetryFallbackApplied { .. }))
            .count(),
        1,
        "the exhausted fallback must not reapply or skip within the chain: {events:?}"
    );
    assert!(events.iter().any(|event| matches!(
        event,
        SessionEvent::AutoRetryEnd {
            success: false,
            attempt: 2,
            ..
        }
    )));
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
    let (result, events) =
        collect_events_during(&session, session.run("stay primary", vec![])).await;
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
    let (session, _, backup_name, primary_reg, backup_reg, _cwd) = make_linked_sessions(
        vec![FauxResponse::error("503 Service unavailable")],
        vec![FauxResponse::text("late-fallback")],
    );
    let settings = session.retry_settings();
    session.set_retry_settings(RetrySettings {
        enabled: true,
        max_retries: 3,
        base_delay_ms: 30_000,
        model_fallback: true,
        fallback_chains: settings.fallback_chains,
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
    let mut fallback_applied = false;
    while let Ok(event) = events.try_recv() {
        fallback_applied |= matches!(event, SessionEvent::RetryFallbackApplied { .. });
    }
    assert!(
        !fallback_applied,
        "cancellation must terminate before fallback"
    );
    let backup = pi_coding::resolve_model(&backup_name).expect("backup model");
    session
        .set_model_with_resolved_auth(backup)
        .await
        .expect("select untouched backup");
    let follow_up = session
        .run("after cancellation", vec![])
        .await
        .expect("backup remains queued");
    assert_eq!(
        follow_up.text, "late-fallback",
        "backup must remain unrequested after cancellation"
    );
    primary_reg.unregister();
    backup_reg.unregister();
}

#[tokio::test(flavor = "current_thread")]
async fn exhausted_diagnostics_are_sanitized() {
    let _guard = REGISTRY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let secret = ["s", "k-", "abc1234567890", "secret"].concat();
    let secret_prefix = ["s", "k-", "abc"].concat();
    let (session, _, _, primary_reg, backup_reg, _cwd) = make_linked_sessions(
        vec![FauxResponse::error(format!(
            "503 upstream api_key={secret} failed"
        ))],
        vec![FauxResponse::error(format!(
            "503 backup Authorization: Bearer {secret}"
        ))],
    );
    let (result, events) = collect_events_during(&session, session.run("exhaust", vec![])).await;
    let error = result.expect_err("both models fail").to_string();
    assert!(!error.contains(secret_prefix.as_str()), "{error}");
    assert!(
        error.contains("[REDACTED]") || error.contains("503"),
        "{error}"
    );
    let terminal = events
        .iter()
        .rev()
        .find_map(|event| match event {
            SessionEvent::AutoRetryEnd {
                success: false,
                final_error: Some(message),
                ..
            } => Some(message),
            _ => None,
        })
        .expect("terminal AutoRetryEnd event");
    assert!(!terminal.contains(secret_prefix.as_str()), "{terminal}");
    assert!(
        terminal.contains("[REDACTED]") || terminal.contains("503"),
        "{terminal}"
    );
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

fn child_definition(name: &str) -> AgentDefinition {
    AgentDefinition {
        name: name.to_owned(),
        description: name.to_owned(),
        system_prompt: "child".to_owned(),
        trusted: true,
        ..AgentDefinition::default()
    }
}

fn child_request_with_sequence(
    definition: AgentDefinition,
    model: Model,
    sequence: Vec<pi_coding::ResolvedAgentModelCandidate>,
    selected_index: usize,
) -> ChildSessionRequest {
    let mut request = child_request(definition, model);
    request.resolved_model_sequence = sequence;
    request.resolved_model_selected_index = selected_index;
    request
}

fn child_request(definition: AgentDefinition, model: Model) -> ChildSessionRequest {
    ChildSessionRequest {
        child_id: "Child".to_owned(),
        parent_id: "Main".to_owned(),
        max_tools_per_agent: 16,
        depth: 1,
        definition,
        assignment: "inspect".to_owned(),
        system_prompt: "child".to_owned(),
        requested_tool_names: Some(Vec::new()),
        orchestration_tools: Vec::new(),
        persona_skill_store: None,
        persona_skills: Arc::new(Vec::new()),
        shared_skills: Arc::new(Vec::new()),
        thinking_level: None,
        model,
        resolved_model_sequence: Vec::new(),
        resolved_model_selected_index: 0,
        yield_state: Arc::new(YieldState::default()),
        output_schema: None,
        schema_mode: None,
    }
}

#[tokio::test(flavor = "current_thread")]
async fn child_session_inherits_parent_retry_settings() {
    let _guard = REGISTRY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (parent, _, _, primary_reg, backup_reg, _cwd) = make_linked_sessions(
        vec![FauxResponse::text("parent-ok")],
        vec![FauxResponse::text("unused")],
    );
    let expected = parent.retry_settings();
    let child = OrchestrationRuntime::child_factory_from_session(&parent)(child_request(
        child_definition("researcher"),
        parent.model().expect("parent model"),
    ))
    .await
    .expect("child");
    assert_eq!(child.retry_settings(), expected);
    assert_eq!(child.retry_role().as_deref(), Some("researcher"));
    primary_reg.unregister();
    backup_reg.unregister();
}

#[tokio::test(flavor = "current_thread")]
async fn child_role_chain_is_used_instead_of_default() {
    let _guard = REGISTRY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (parent, primary_name, backup_name, primary_reg, backup_reg, _cwd) = make_linked_sessions(
        vec![FauxResponse::error("503 Service unavailable")],
        vec![FauxResponse::text("role-fallback-ok")],
    );
    let mut chains = RetryFallbackChains::default();
    chains.insert("default".into(), vec!["unused/model".into()]);
    chains.insert("researcher".into(), vec![backup_name.clone()]);
    parent.set_retry_settings(RetrySettings {
        enabled: true,
        max_retries: 0,
        base_delay_ms: 1,
        model_fallback: true,
        fallback_chains: chains,
        ..Default::default()
    });

    let child = OrchestrationRuntime::child_factory_from_session(&parent)(child_request(
        child_definition("researcher"),
        parent.model().expect("parent model"),
    ))
    .await
    .expect("child");
    let (result, events) = collect_events_during(&child, child.run("switch please", vec![])).await;
    let result = result.expect("role fallback success");
    assert_eq!(result.text, "role-fallback-ok");
    let primary_selector = format!("{primary_name}:off");
    let fallback_selector = format!("{backup_name}:off");
    assert!(
        events.iter().any(|event| matches!(
            event,
            SessionEvent::RetryFallbackApplied { from, to, role }
                if from == &primary_selector
                    && to == &fallback_selector
                    && role == "researcher"
        )),
        "missing researcher applied event in {events:?}"
    );
    assert_eq!(
        child.retry_fallback_model().as_deref(),
        Some(fallback_selector.as_str())
    );
    primary_reg.unregister();
    backup_reg.unregister();
}

#[tokio::test(flavor = "current_thread")]
async fn nested_child_uses_its_own_definition_retry_role() {
    let _guard = REGISTRY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (parent, _, _, primary_reg, backup_reg, _cwd) = make_linked_sessions(
        vec![FauxResponse::text("parent-ok")],
        vec![FauxResponse::text("unused")],
    );
    let mut snapshot = parent.child_session_options_snapshot();
    snapshot.retry_role = Some("researcher".to_owned());
    let child = OrchestrationRuntime::child_factory_from_snapshot(snapshot)(child_request(
        child_definition("reviewer"),
        parent.model().expect("parent model"),
    ))
    .await
    .expect("nested child");
    assert_eq!(child.retry_role().as_deref(), Some("reviewer"));
    primary_reg.unregister();
    backup_reg.unregister();
}

#[tokio::test(flavor = "current_thread")]
async fn child_definition_model_list_falls_back_in_order_and_sticks() {
    let _guard = REGISTRY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (parent, primary_name, backup_name, primary_reg, backup_reg, _cwd) = make_linked_sessions(
        vec![FauxResponse::error("503 primary unavailable")],
        vec![
            FauxResponse::text("fallback-first-turn"),
            FauxResponse::text("fallback-second-turn"),
        ],
    );
    parent.set_retry_settings(RetrySettings {
        enabled: true,
        max_retries: 0,
        base_delay_ms: 1,
        model_fallback: true,
        fallback_chains: RetryFallbackChains::default(),
    });
    let definition = child_definition("researcher");
    let primary = parent.model().expect("parent model");
    let backup = pi_coding::resolve_model(&backup_name).expect("backup model");
    let sequence = vec![
        pi_coding::ResolvedAgentModelCandidate {
            model: primary.clone(),
            selector: primary_name.clone(),
            thinking_level: None,
        },
        pi_coding::ResolvedAgentModelCandidate {
            model: backup,
            selector: backup_name.clone(),
            thinking_level: None,
        },
    ];
    let request = child_request_with_sequence(definition, primary, sequence, 0);
    let child = OrchestrationRuntime::child_factory_from_session(&parent)(request)
        .await
        .expect("child");

    let first = child
        .run("first turn", vec![])
        .await
        .expect("definition fallback succeeds");
    assert_eq!(first.text, "fallback-first-turn");
    let backup_provider = backup_name.split('/').next().expect("backup provider");
    assert_eq!(
        child.model().expect("switched model").provider,
        backup_provider
    );

    let second = child
        .run("second turn", vec![])
        .await
        .expect("switched model remains active");
    assert_eq!(second.text, "fallback-second-turn");
    let fallback_selector = format!("{backup_name}:off");
    assert_eq!(
        child.retry_fallback_model().as_deref(),
        Some(fallback_selector.as_str())
    );
    primary_reg.unregister();
    backup_reg.unregister();
}

#[tokio::test(flavor = "current_thread")]
async fn orchestration_definition_model_list_falls_back_in_order() {
    let _guard = REGISTRY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (parent, primary_name, backup_name, primary_reg, backup_reg, artifacts) =
        make_linked_sessions(
            vec![FauxResponse::error("503 primary unavailable")],
            vec![
                FauxResponse::text("boundary-fallback-first"),
                FauxResponse::text("boundary-fallback-second"),
            ],
        );
    parent.set_retry_settings(RetrySettings {
        enabled: true,
        max_retries: 0,
        base_delay_ms: 1,
        model_fallback: true,
        fallback_chains: RetryFallbackChains::default(),
    });
    let definition = AgentDefinition {
        name: "researcher".to_owned(),
        description: "researcher".to_owned(),
        system_prompt: "child".to_owned(),
        tools: Some(Vec::new()),
        model: Some(vec![primary_name.clone(), backup_name.clone()]),
        thinking_level: Some(ThinkingLevel::Off),
        source: AgentDefinitionSource::Bundled,
        trusted: true,
        ..AgentDefinition::default()
    };
    let mut config = OrchestrationConfig::new(
        AgentCatalog::from_agents(vec![definition]),
        artifacts.path().join("artifacts"),
    )
    .with_parent_model(parent.model().expect("parent model"));
    config.default_agent = "researcher".to_owned();
    let runtime = OrchestrationRuntime::new(
        config,
        OrchestrationRuntime::child_factory_from_session(&parent),
    )
    .expect("orchestration runtime");
    let spawn = runtime
        .spawn_tasks(
            "Main",
            0,
            vec![TaskItem {
                index: 0,
                id: "DefinitionFallback".to_owned(),
                agent: "researcher".to_owned(),
                assignment: "first turn".to_owned(),
                ..TaskItem::default()
            }],
        )
        .expect("spawn from definition")
        .remove(0);
    let settled = runtime
        .wait_jobs(
            &[spawn.job_id.clone()],
            Some(std::time::Duration::from_secs(5)),
            None,
        )
        .await
        .expect("definition-backed job settles");
    assert_eq!(
        settled[0].status,
        JobStatus::Completed,
        "{:?}",
        settled[0].result
    );
    assert!(
        settled[0]
            .result
            .as_ref()
            .is_some_and(|task| task.output.contains("boundary-fallback-first")),
        "unexpected result: {:?}",
        settled[0].result
    );
    runtime.shutdown().await;
    primary_reg.unregister();
    backup_reg.unregister();
}

#[tokio::test(flavor = "current_thread")]
async fn child_non_retryable_error_does_not_advance_model_list() {
    let _guard = REGISTRY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (parent, primary_name, backup_name, primary_reg, backup_reg, _cwd) = make_linked_sessions(
        vec![FauxResponse::error("invalid request")],
        vec![FauxResponse::text("must-not-run")],
    );
    parent.set_retry_settings(RetrySettings {
        enabled: true,
        max_retries: 0,
        base_delay_ms: 1,
        model_fallback: true,
        fallback_chains: RetryFallbackChains::default(),
    });
    let definition = child_definition("researcher");
    let primary = parent.model().expect("parent model");
    let backup = pi_coding::resolve_model(&backup_name).expect("backup model");
    let sequence = vec![
        pi_coding::ResolvedAgentModelCandidate {
            model: primary.clone(),
            selector: primary_name.clone(),
            thinking_level: None,
        },
        pi_coding::ResolvedAgentModelCandidate {
            model: backup,
            selector: backup_name.clone(),
            thinking_level: None,
        },
    ];
    let child = OrchestrationRuntime::child_factory_from_session(&parent)(
        child_request_with_sequence(definition, primary, sequence, 0),
    )
    .await
    .expect("child");

    let error = child
        .run("invalid", vec![])
        .await
        .expect_err("non-retryable failure");
    assert!(error.to_string().contains("invalid request"));
    assert!(child.retry_fallback_model().is_none());
    primary_reg.unregister();
    backup_reg.unregister();
}

#[tokio::test(flavor = "current_thread")]
async fn child_skips_model_list_candidate_without_credentials() {
    let _guard = REGISTRY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (parent, primary_name, backup_name, primary_reg, backup_reg, _cwd) = make_linked_sessions(
        vec![FauxResponse::error("503 primary unavailable")],
        vec![FauxResponse::text("must-not-run")],
    );
    let unavailable = model("unavailable-provider", "missing-credentials");
    let definition = child_definition("researcher");
    let primary = parent.model().expect("parent model");
    let backup = pi_coding::resolve_model(&backup_name).expect("backup model");
    let sequence = vec![
        pi_coding::ResolvedAgentModelCandidate {
            model: primary.clone(),
            selector: primary_name.clone(),
            thinking_level: None,
        },
        pi_coding::ResolvedAgentModelCandidate {
            model: unavailable.clone(),
            selector: format!("{}/{}", unavailable.provider, unavailable.id),
            thinking_level: None,
        },
        pi_coding::ResolvedAgentModelCandidate {
            model: backup,
            selector: backup_name.clone(),
            thinking_level: None,
        },
    ];
    let child = OrchestrationRuntime::child_factory_from_session(&parent)(
        child_request_with_sequence(definition, primary, sequence, 0),
    )
    .await
    .expect("child");

    let result = child
        .run("fallback", vec![])
        .await
        .expect("later credentialed candidate succeeds");
    assert_eq!(result.text, "must-not-run");
    assert_eq!(
        child.model().expect("fallback model").provider,
        backup_name.split('/').next().unwrap()
    );
    primary_reg.unregister();
    backup_reg.unregister();
}

#[tokio::test(flavor = "current_thread")]
async fn child_weekly_limit_uses_configured_sequence_with_retry_and_credential_skip() {
    let _guard = REGISTRY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let suffix = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let primary = model(&format!("weekly-primary-{suffix}"), "main");
    let missing = model(&format!("missing-{suffix}"), "no-key");
    let deepseek = model(&format!("deepseek-{suffix}"), "reasoner");
    let final_model = model(&format!("final-{suffix}"), "reserve");
    let primary_name = format!("{}/{}", primary.provider, primary.id);
    let missing_name = format!("{}/{}", missing.provider, missing.id);
    let deepseek_name = format!("{}/{}", deepseek.provider, deepseek.id);
    let final_name = format!("{}/{}", final_model.provider, final_model.id);
    let primary_reg = register_faux_provider(FauxProviderOptions {
        api: primary.api.clone(),
        provider: primary.provider.clone(),
        models: vec![primary.clone()],
        chunk_size: 64,
    });
    primary_reg.set_responses(vec![FauxResponse::error(
        "HTTP 429 usage/quota exhausted: Weekly usage limit reached",
    )]);
    let missing_reg = register_faux_provider(FauxProviderOptions {
        api: missing.api.clone(),
        provider: missing.provider.clone(),
        models: vec![missing.clone()],
        chunk_size: 64,
    });
    missing_reg.set_responses(vec![FauxResponse::text(
        "missing credentials candidate must not run",
    )]);
    let deepseek_reg = register_faux_provider(FauxProviderOptions {
        api: deepseek.api.clone(),
        provider: deepseek.provider.clone(),
        models: vec![deepseek.clone()],
        chunk_size: 64,
    });
    deepseek_reg.set_responses(vec![
        FauxResponse::error("503 DeepSeek provider unavailable"),
        FauxResponse::error("503 DeepSeek provider unavailable after retry"),
    ]);
    let final_reg = register_faux_provider(FauxProviderOptions {
        api: final_model.api.clone(),
        provider: final_model.provider.clone(),
        models: vec![final_model.clone()],
        chunk_size: 64,
    });
    final_reg.set_responses(vec![FauxResponse::text("child ordered fallback succeeded")]);

    let primary_provider = primary.provider.clone();
    let missing_provider = missing.provider.clone();
    let deepseek_provider = deepseek.provider.clone();
    let final_provider = final_model.provider.clone();
    let cwd = tempfile::tempdir().expect("child fallback cwd");
    let parent = Session::new(SessionOptions {
        model: primary.clone(),
        cwd: cwd.path().to_path_buf(),
        system_prompt: String::new(),
        thinking_level: ThinkingLevel::Off,
        api_key: "primary-key".to_owned(),
        compaction: None,
        stream_options: SimpleStreamOptions::default(),
        tools: Some(Vec::new()),
        before_tool_call: None,
        after_tool_call: None,
        stream_fn: None,
        auth_resolver: Some(Arc::new(move |requested: Model| {
            let primary_provider = primary_provider.clone();
            let missing_provider = missing_provider.clone();
            let deepseek_provider = deepseek_provider.clone();
            let final_provider = final_provider.clone();
            Box::pin(async move {
                if requested.provider == missing_provider {
                    return Err(anyhow::anyhow!("credentials unavailable"));
                }
                if ![primary_provider, deepseek_provider, final_provider]
                    .contains(&requested.provider)
                {
                    return Err(anyhow::anyhow!("unexpected model {}", requested.provider));
                }
                Ok(pi_coding::RequestAuth {
                    api_key: "credentialed-key".to_owned(),
                    headers: Default::default(),
                    env: Default::default(),
                    available_model_ids: None,
                    kind: pi_coding::RequestAuthKind::ApiKeyOrHeaders,
                })
            })
        })),
        auth_manager: None,
    })
    .expect("parent session");
    parent.set_retry_settings(RetrySettings {
        enabled: true,
        max_retries: 1,
        base_delay_ms: 1,
        model_fallback: true,
        fallback_chains: RetryFallbackChains::default(),
    });
    let definition = AgentDefinition {
        name: "researcher".to_owned(),
        description: "researcher".to_owned(),
        system_prompt: "child".to_owned(),
        tools: Some(Vec::new()),
        model: Some(vec![
            primary_name.clone(),
            missing_name.clone(),
            deepseek_name.clone(),
            final_name.clone(),
        ]),
        thinking_level: Some(ThinkingLevel::Off),
        source: AgentDefinitionSource::Bundled,
        trusted: true,
        ..AgentDefinition::default()
    };
    let mut config = OrchestrationConfig::new(
        AgentCatalog::from_agents(vec![definition]),
        cwd.path().join("artifacts"),
    )
    .with_parent_model(primary.clone());
    config.default_agent = "researcher".to_owned();
    let child_factory = OrchestrationRuntime::child_factory_from_session(&parent);
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    let capturing_factory: pi_coding::ChildSessionFactory =
        Arc::new(move |request: ChildSessionRequest| {
            let child_factory = child_factory.clone();
            let event_tx = event_tx.clone();
            Box::pin(async move {
                let child = child_factory(request).await?;
                let mut events = child.subscribe_session_events();
                tokio::spawn(async move {
                    while let Ok(event) = events.recv().await {
                        if event_tx.send(event).is_err() {
                            break;
                        }
                    }
                });
                Ok(child)
            })
        });
    let runtime =
        OrchestrationRuntime::new(config, capturing_factory).expect("orchestration runtime");
    let spawn = runtime
        .spawn_tasks(
            "Main",
            0,
            vec![TaskItem {
                index: 0,
                id: "ConfiguredFallback".to_owned(),
                agent: "researcher".to_owned(),
                assignment: "exercise configured fallback".to_owned(),
                ..TaskItem::default()
            }],
        )
        .expect("spawn from AgentDefinition")
        .remove(0);
    let settled = runtime
        .wait_jobs(
            &[spawn.job_id],
            Some(std::time::Duration::from_secs(5)),
            None,
        )
        .await
        .expect("definition-backed job settles");
    assert_eq!(
        settled[0].status,
        JobStatus::Completed,
        "{:?}",
        settled[0].result
    );
    assert!(
        settled[0]
            .result
            .as_ref()
            .is_some_and(|task| task.output.contains("child ordered fallback succeeded")),
        "unexpected result: {:?}",
        settled[0].result
    );

    let mut events = Vec::new();
    loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), event_rx.recv())
            .await
            .expect("child retry event timeout")
            .expect("child retry event stream");
        let is_success = matches!(event, SessionEvent::RetryFallbackSucceeded { .. });
        events.push(event);
        if is_success {
            while let Ok(event) = event_rx.try_recv() {
                events.push(event);
            }
            break;
        }
    }
    let recovery = events
        .iter()
        .filter_map(|event| match event {
            SessionEvent::RetryFallbackApplied { from, to, .. } => {
                Some(format!("apply:{from}->{to}"))
            }
            SessionEvent::AutoRetryStart {
                attempt,
                max_attempts,
                ..
            } => Some(format!("retry:{attempt}/{max_attempts}")),
            SessionEvent::RetryFallbackSucceeded { model, role } => {
                Some(format!("success:{model}:{role}"))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        recovery,
        vec![
            format!("apply:{primary_name}:off->{deepseek_name}:off"),
            "retry:1/1".to_owned(),
            format!("apply:{deepseek_name}:off->{final_name}:off"),
            format!("success:{final_name}:off:researcher"),
        ],
        "unexpected child retry/fallback receipts: {events:?}"
    );
    assert!(events.iter().all(|event| !matches!(event, SessionEvent::RetryFallbackApplied { to, .. } if to.contains(&missing_name))));
    runtime.shutdown().await;

    primary_reg.unregister();
    missing_reg.unregister();
    deepseek_reg.unregister();
    final_reg.unregister();
}
