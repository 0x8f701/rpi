// Empirical performance measurements for the process-hosted bun extension path.
// Run: PI_BUN_EXECUTABLE=<bun-path> cargo test -p pi-coding --release --test extension_perf -- --nocapture

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use pi_coding::extensions::{
    ExtensionCapability, ExtensionMode, ExtensionOrigin, ExtensionPermissionSet,
    ExtensionRuntime, ExtensionRuntimeOptions, ExtensionSpec, ExtensionSpecRuntime,
};
use pi_coding::ExtensionEvent;
use pi_agent::AbortSignal;
use serde_json::{json, Value};
use tokio::time::timeout;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/extensions");
const N_LOAD: usize = 5;
const N_INVOKE: usize = 2000;
const N_TOOL: usize = 2000;
const N_STARTUP_EXTENSIONS: usize = 5;
const N_EVENT: usize = 2000;

fn bun_executable() -> Option<PathBuf> {
    if let Some(configured) = std::env::var_os("PI_BUN_EXECUTABLE") {
        let configured = PathBuf::from(configured);
        if configured.is_file() {
            return Some(configured);
        }
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join(if cfg!(windows) { "bun.exe" } else { "bun" }))
        .find(|candidate| candidate.is_file())
}

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

fn bun_spec(bun: &Path) -> ExtensionSpec {
    let entry = PathBuf::from(FIXTURES).join("perf-bench.ts");
    let mut spec = ExtensionSpec::new_runtime(
        "perf-bench",
        ExtensionSpecRuntime::Bun { entry },
        PathBuf::from(FIXTURES),
        ExtensionOrigin::Project,
        true,
        ExtensionPermissionSet {
            capabilities: BTreeSet::from([
                ExtensionCapability::Commands,
                ExtensionCapability::Tools,
                ExtensionCapability::EventHooks,
            ]),
            ui_capabilities: BTreeSet::new(),
        },
    );
    spec.environment.insert(
        "PI_BUN_EXECUTABLE".to_owned(),
        bun.to_string_lossy().into_owned(),
    );
    spec
}

fn percentile(mut samples: Vec<f64>, p: f64) -> f64 {
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let idx = ((samples.len() as f64) * p).ceil() as usize - 1;
    samples[idx.min(samples.len() - 1)]
}

#[tokio::test]
async fn perf_bun_extension_paths() -> Result<(), Box<dyn std::error::Error>> {
    let Some(bun) = bun_executable() else {
        eprintln!("bun not found; skipping");
        return Ok(());
    };

    // --- 1. load (spawn + handshake + load + initialize) ---
    let mut load_times: Vec<f64> = Vec::new();
    for _ in 0..N_LOAD {
        let runtime = ExtensionRuntime::process(None, options());
        let start = Instant::now();
        let report = runtime.load(vec![bun_spec(&bun)]).await;
        let elapsed = start.elapsed();
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        load_times.push(elapsed.as_secs_f64() * 1000.0);
        runtime.shutdown().await;
    }
    eprintln!(
        "load(spawn+handshake+load+init): n={} avg={:.2}ms min={:.2}ms max={:.2}ms",
        load_times.len(),
        load_times.iter().sum::<f64>() / load_times.len() as f64,
        load_times.iter().cloned().fold(f64::INFINITY, f64::min),
        load_times.iter().cloned().fold(0.0f64, f64::max),
    );

    let startup_specs = (0..N_STARTUP_EXTENSIONS)
        .map(|index| {
            let mut spec = bun_spec(&bun);
            spec.id = format!("perf-bench-{index}");
            spec
        })
        .collect::<Vec<_>>();
    let startup_runtime = ExtensionRuntime::process(None, options());
    let start = Instant::now();
    let startup_report = startup_runtime.load(startup_specs).await;
    assert!(startup_report.failures.is_empty(), "{:?}", startup_report.failures);
    eprintln!(
        "parallel startup: n={} total={:.2}ms",
        N_STARTUP_EXTENSIONS,
        start.elapsed().as_secs_f64() * 1000.0
    );
    startup_runtime.shutdown().await;

    // --- steady-state runtime for invoke benchmarks ---
    let runtime = ExtensionRuntime::process(None, options());
    let report = runtime.load(vec![bun_spec(&bun)]).await;
    assert!(report.failures.is_empty(), "{:?}", report.failures);

    // --- 2. command invocation round trip ---
    let mut cmd_samples: Vec<f64> = Vec::with_capacity(N_INVOKE);
    for _ in 0..N_INVOKE {
        let start = Instant::now();
        runtime
            .invoke_command("noop", String::new(), None, None)
            .await?;
        cmd_samples.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    report_ms("command round trip", &cmd_samples);

    // --- 3. tool invocation round trip (args + AgentToolResult decode) ---
    let mut tool_samples: Vec<f64> = Vec::with_capacity(N_TOOL);
    for _ in 0..N_TOOL {
        let start = Instant::now();
        runtime
            .invoke_tool("noop_tool", "call-1".to_owned(), json!({}), AbortSignal::none(), None)
            .await?;
        tool_samples.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    report_ms("tool round trip", &tool_samples);

    // --- 4. event hook round trip ---
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

    // --- 5. throughput: back-to-back invocations ---
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

    // --- 6. shutdown ---
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
