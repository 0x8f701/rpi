// Empirical performance measurements for the in-process QuickJS extension
// path. Run: cargo test -p pi-coding --release --test extension_perf -- --nocapture

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use pi_agent::AbortSignal;
use pi_ai::Model;
use pi_ai::providers::{FauxProviderOptions, FauxProviderRegistration, register_faux_provider};
use pi_coding::{
    Application, ApplicationRuntimeCandidate, ExtensionCapability, ExtensionEvent, ExtensionMode,
    ExtensionPermissionSet, ExtensionRuntime, ExtensionRuntimeOptions, ResourceManager,
    ResourceManagerOptions, Session, SessionOptions,
};
use serde_json::json;
use tokio::time::timeout;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/extensions");
const N_LOAD: usize = 5;
const N_INVOKE: usize = 2000;
const N_TOOL: usize = 2000;
const N_STARTUP_EXTENSIONS: usize = 5;
const N_EVENT: usize = 2000;

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

fn permissions() -> ExtensionPermissionSet {
    ExtensionPermissionSet {
        capabilities: BTreeSet::from([
            ExtensionCapability::Commands,
            ExtensionCapability::Tools,
            ExtensionCapability::EventHooks,
        ]),
        ui_capabilities: BTreeSet::new(),
    }
}

/// A session model with a resolvable provider, required since the sandbox
/// topology stages the session HTTP authorities and `resolve_provider_http_authorities`
/// fails for a `Model::default()` (empty `api`). The canonical hermetic faux
/// pattern mirrors `extensions::bootstrap_extension_application`.
fn faux_session_model() -> (Model, FauxProviderRegistration) {
    let suffix = uuid::Uuid::now_v7();
    let model = Model {
        id: format!("perf-fixture-model-{suffix}"),
        name: "Perf Fixture Model".to_owned(),
        api: format!("perf-fixture-api-{suffix}"),
        provider: format!("perf-fixture-provider-{suffix}"),
        base_url: "http://localhost:0".to_owned(),
        ..Model::default()
    };
    let registration = register_faux_provider(FauxProviderOptions {
        api: model.api.clone(),
        provider: model.provider.clone(),
        models: vec![model.clone()],
        chunk_size: 1,
    });
    (model, registration)
}

async fn bootstrap_runtime(
    extension_count: usize,
) -> Result<
    (
        Application,
        ExtensionRuntime,
        tempfile::TempDir,
        FauxProviderRegistration,
    ),
    Box<dyn std::error::Error>,
> {
    let root = tempfile::tempdir()?;
    let project = root.path().join("project");
    std::fs::create_dir_all(&project)?;
    let fixture = PathBuf::from(FIXTURES).join("quickjs-perf-bench.mjs");
    let mut manifests = Vec::with_capacity(extension_count);
    for index in 0..extension_count {
        let id = if extension_count == 1 {
            "perf-bench".to_owned()
        } else {
            format!("perf-bench-{index}")
        };
        let package = project.join(&id);
        std::fs::create_dir_all(&package)?;
        std::fs::copy(&fixture, package.join("quickjs-perf-bench.mjs"))?;
        let manifest = package.join(pi_coding::PROCESS_EXTENSION_MANIFEST_FILE);
        std::fs::write(
            &manifest,
            serde_json::to_vec_pretty(&json!({
                "schemaVersion": 1,
                "id": id,
                "runtime": "quickjs",
                "entry": "quickjs-perf-bench.mjs",
                "capabilities": ["commands", "tools", "event_hooks"]
            }))?,
        )?;
        manifests.push(manifest);
    }
    let (model, faux) = faux_session_model();
    let session_dir = project.join("sessions");
    std::fs::create_dir_all(&session_dir)?;
    let session = Session::new_with_additional_tools_filtered_and_discovery(
        SessionOptions {
            model,
            cwd: project.clone(),
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
        },
        Vec::new(),
        pi_coding::ToolSelection::default(),
        pi_coding::ResourceDiscovery::Disabled,
    )?;
    session.set_session_dir(session_dir);
    let mut resource_options = ResourceManagerOptions::new(&project);
    resource_options.agent_dir = root.path().join("agent");
    resource_options.project_trust_override = Some(true);
    resource_options.disable_skills = true;
    resource_options.disable_prompt_templates = true;
    resource_options.disable_themes = true;
    resource_options.disable_context_files = true;
    std::fs::create_dir_all(&resource_options.agent_dir)?;
    resource_options.explicit_extension_paths = manifests;
    // Pin the sandbox to danger-full-access so Application bootstrap does not
    // authenticate the confined bubblewrap launcher nor require a host HTTP
    // destination policy; the perf bench measures extension paths only.
    std::fs::write(
        resource_options.agent_dir.join("settings.json"),
        r#"{"sandbox":{"mode":"danger-full-access"}}"#,
    )?;
    std::fs::create_dir_all(project.join("sessions"))?;
    session
        .attach_resources(ResourceManager::new(resource_options)?)
        .await?;
    let runtime = ExtensionRuntime::process(None, options());
    let application = Application::from_runtime_candidate(
        ApplicationRuntimeCandidate::new(session).with_extensions(runtime.clone(), permissions()),
    )
    .await?;
    Ok((application, runtime, root, faux))
}

fn percentile(mut samples: Vec<f64>, p: f64) -> f64 {
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let idx = ((samples.len() as f64) * p).ceil() as usize - 1;
    samples[idx.min(samples.len() - 1)]
}

#[tokio::test]
async fn perf_quickjs_extension_paths() -> Result<(), Box<dyn std::error::Error>> {
    let mut load_times: Vec<f64> = Vec::new();
    for _ in 0..N_LOAD {
        let start = Instant::now();
        let (application, runtime, _project, _faux) = bootstrap_runtime(1).await?;
        let elapsed = start.elapsed();
        load_times.push(elapsed.as_secs_f64() * 1000.0);
        application.cleanup().await;
        runtime.shutdown().await;
    }
    eprintln!(
        "load(runtime+bootstrap+load+init): n={} avg={:.2}ms min={:.2}ms max={:.2}ms",
        load_times.len(),
        load_times.iter().sum::<f64>() / load_times.len() as f64,
        load_times.iter().cloned().fold(f64::INFINITY, f64::min),
        load_times.iter().cloned().fold(0.0f64, f64::max),
    );

    let start = Instant::now();
    let (startup_application, startup_runtime, _project, _faux) =
        bootstrap_runtime(N_STARTUP_EXTENSIONS).await?;
    eprintln!(
        "parallel startup: n={} total={:.2}ms",
        N_STARTUP_EXTENSIONS,
        start.elapsed().as_secs_f64() * 1000.0
    );
    startup_application.cleanup().await;
    startup_runtime.shutdown().await;

    let (application, runtime, _project, _faux) = bootstrap_runtime(1).await?;

    let mut cmd_samples: Vec<f64> = Vec::with_capacity(N_INVOKE);
    for _ in 0..N_INVOKE {
        let start = Instant::now();
        runtime
            .invoke_command("noop", String::new(), None, None)
            .await?;
        cmd_samples.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    report_ms("command round trip", &cmd_samples);

    let mut tool_samples: Vec<f64> = Vec::with_capacity(N_TOOL);
    for _ in 0..N_TOOL {
        let start = Instant::now();
        runtime
            .invoke_tool(
                "noop_tool",
                "call-1".to_owned(),
                json!({}),
                AbortSignal::none(),
                None,
            )
            .await?;
        tool_samples.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    report_ms("tool round trip", &tool_samples);

    let mut event_samples: Vec<f64> = Vec::with_capacity(N_EVENT);
    for _ in 0..N_EVENT {
        let start = Instant::now();
        let outcomes = runtime
            .emit(ExtensionEvent::new("turn_start", json!({})))
            .await;
        assert_eq!(outcomes.len(), 1);
        event_samples.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    report_ms("event hook round trip", &event_samples);

    let start = Instant::now();
    for _ in 0..N_INVOKE {
        runtime
            .invoke_command("noop", String::new(), None, None)
            .await?;
    }
    let total = start.elapsed();
    eprintln!(
        "throughput: {} invocations in {:.2}s = {:.0} invocations/sec",
        N_INVOKE,
        total.as_secs_f64(),
        N_INVOKE as f64 / total.as_secs_f64()
    );

    application.cleanup().await;
    let start = Instant::now();
    let shutdown_result = timeout(Duration::from_secs(5), runtime.shutdown()).await;
    eprintln!(
        "shutdown: {:.2}ms ({:?})",
        start.elapsed().as_secs_f64() * 1000.0,
        shutdown_result.is_ok()
    );

    Ok(())
}

fn report_ms(label: &str, samples: &[f64]) {
    let n = samples.len() as f64;
    let avg = samples.iter().sum::<f64>() / n;
    eprintln!(
        "{label}: n={} avg={:.3}ms p50={:.3}ms p95={:.3}ms p99={:.3}ms max={:.3}ms",
        samples.len(),
        avg,
        percentile(samples.to_vec(), 0.50),
        percentile(samples.to_vec(), 0.95),
        percentile(samples.to_vec(), 0.99),
        samples.iter().cloned().fold(0.0f64, f64::max),
    );
}
