//! New-generation governed sandbox boundary smoke tests.
//!
//! This file restores the boundary coverage of the deleted legacy
//! `sandbox_smoke.rs` (which drove the old `unshare`/`execute_bash` sandbox)
//! against the current governed `Application`/generation architecture.
//!
//! Boundaries are exercised through the live product path only:
//!
//! - a real `Application` bootstraps from `<agent_dir>/settings.json` through
//!   `Application::from_runtime_candidate` (fallible product constructor,
//!   revision-one bootstrap, immutable `ApplicationGeneration`);
//! - tools come exclusively from `application.get_all_tools()` — generation
//!   bound (the capture sits inside the published session), never from
//!   legacy selectors (`create_tool`, `execute_bash`, `SandboxConfigFn`,
//!   `sandbox::resolve`, per-call `sandboxed`);
//! - governed subagent confinement goes through the production
//!   `OrchestrationRuntime::child_factory_from_session` child factory, whose
//!   child tools inherit the parent session's generation capture.
//!
//! Deterministic tier (always run): `sandbox.mode = danger-full-access`
//! skips the trusted launcher probe entirely, so the same scaffold runs
//! identically on the static-gates runner and any host. These tests prove
//! generation-bound workspace write/read, capability denies outside the
//! workspace, hostile literal path handling (typed rejection or literal
//! policy strings — never evaluation), and settings-mode resolution/serde
//! precedence.
//!
//! Live confinement tier runs through the separate harnessless
//! `sandbox_confined` target so the production hidden-child argv is dispatched
//! before libtest initialization. Confined modes (`workspace-write`,
//! `read-only`) authenticate the trusted bubblewrap launcher during bootstrap.
//! On a host without a trusted launcher the gate reports the explicit
//! unsupported status and returns (never a fake pass). These tests prove
//! confined workspace write, filesystem denies, read-only mounts, the private
//! HOME/TMPDIR/scratch projections for governed subprocesses, network-none
//! child namespaces, hostile absolute denied-path literals, settings reloads,
//! timeout descendant reaping, and governed subagent confinement.
//!
//! Linux-only: the governed brush/bash path and the bubblewrap launcher are
//! the Linux sandbox surface (the deleted file was Linux-specific too).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use pi_agent::{
    AbortController, AgentTool, AgentToolResult, ThinkingLevel, ToolCallContext, ToolUpdateFn,
};
use pi_ai::providers::{FauxProviderOptions, FauxProviderRegistration, register_faux_provider};
use pi_ai::{ContentBlock, Model};
use pi_coding::{
    AgentDefinitionSource, Application, ApplicationRuntimeCandidate, ResourceDiscovery,
    ResourceManager, ResourceManagerOptions, SandboxSettingsWire, Session, SessionOptions,
    WorkspaceRoots, resolved_sandbox_mode,
};
use pi_sandbox::{AbsolutePath, SandboxMode};
use serde_json::{Value, json};
use tempfile::TempDir;

/// Danger mode skips the launcher probe, so bootstrap succeeds on any host;
/// the faux provider resolves no HTTP authorities, so the danger-mode
/// requirement of an empty host-http policy holds.
const DANGER_SETTINGS: &str = r#"{"sandbox":{"mode":"danger-full-access"}}"#;

/// Explicit confined workspace-write mode.
const WORKSPACE_WRITE_SETTINGS: &str = r#"{"sandbox":{"mode":"workspace-write"}}"#;

/// Confined read-only mode: workspace roots mount read-only.
const READ_ONLY_SETTINGS: &str = r#"{"sandbox":{"mode":"read-only"}}"#;

// ---------------------------------------------------------------------------
// Fixture (deterministic on every host)
// ---------------------------------------------------------------------------

/// Builds a deterministic session + resource manager. The settings file lives
/// in the agent home so `SettingsManager::load_phase_one` (used by bootstrap
/// and reload) picks it up without touching the real user agent dir. The
/// session constructor uses the default (`None`) tool set, so the session
/// builds the full generation-bound coding tool set (bash/read/write/...)
/// that becomes live when the Application publishes its generation.
fn fixture(
    agent_settings: Option<&str>,
) -> (TempDir, Session, FauxProviderRegistration, ResourceManager) {
    let root = tempfile::tempdir().expect("fixture root");
    let agent_dir = root.path().join("agent-home");
    let cwd = root.path().join("cwd");
    let session_dir = root.path().join("sessions");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    std::fs::create_dir_all(&cwd).expect("cwd");
    std::fs::create_dir_all(&session_dir).expect("sessions");
    if let Some(settings) = agent_settings {
        std::fs::write(agent_dir.join("settings.json"), settings).expect("settings file");
    }

    let suffix = uuid::Uuid::now_v7().to_string();
    let api = format!("sandbox-smoke-api-{suffix}");
    let provider = format!("sandbox-smoke-provider-{suffix}");
    let model = Model {
        id: "sandbox-smoke-model".to_owned(),
        name: "Sandbox Smoke Model".to_owned(),
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

    let mut options = ResourceManagerOptions::new(&cwd);
    options.agent_dir = agent_dir.clone();
    options.disable_extensions = true;
    options.disable_skills = true;
    options.disable_prompt_templates = true;
    options.disable_themes = true;
    options.disable_context_files = true;
    let resources = ResourceManager::new(options).expect("resource manager");

    let session = Session::new_with_additional_tools_filtered_discovery_workspace_and_uri(
        SessionOptions {
            model,
            cwd,
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: String::new(),
            compaction: None,
            stream_options: Default::default(),
            tools: None,
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: None,
            auth_resolver: None,
            auth_manager: None,
        },
        Vec::new(),
        pi_coding::ToolSelection::default(),
        ResourceDiscovery::Disabled,
        WorkspaceRoots::new(&root.path().join("cwd"), Vec::<PathBuf>::new()).expect("workspace"),
        None,
    )
    .expect("session");
    session.set_session_dir(session_dir);
    (root, session, registration, resources)
}

/// Bootstraps a live application through the fallible product constructor.
async fn bootstrap(
    agent_settings: Option<&str>,
) -> (
    TempDir,
    Application,
    FauxProviderRegistration,
    ResourceManager,
) {
    let (root, session, registration, resources) = fixture(agent_settings);
    session
        .attach_resources(resources.clone())
        .await
        .expect("attach resources");
    let application =
        Application::from_runtime_candidate(ApplicationRuntimeCandidate::new(session))
            .await
            .expect("bootstrap application");
    (root, application, registration, resources)
}

/// Extracts the error text from a fallible bootstrap. The `Ok` payload
/// (`Application`) does not implement `Debug`, so `Result::expect_err`
/// cannot be used.
async fn bootstrap_err(
    agent_settings: Option<&str>,
) -> (TempDir, FauxProviderRegistration, ResourceManager, String) {
    let (root, session, registration, resources) = fixture(agent_settings);
    session
        .attach_resources(resources.clone())
        .await
        .expect("attach resources");
    let result =
        Application::from_runtime_candidate(ApplicationRuntimeCandidate::new(session)).await;
    match result {
        Err(error) => (root, registration, resources, format!("{error:#}")),
        Ok(_) => panic!("expected bootstrap to fail for settings: {agent_settings:?}"),
    }
}

pub(crate) const BUBBLEWRAP_NOT_INSTALLED: &str = "bubblewrap not installed";
pub(crate) const FORCED_UNSUPPORTED: &str = "PI_TEST_SANDBOX_FORCE_UNSUPPORTED is set";

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum ConfinedLauncherGate {
    Run,
    Skip(&'static str),
    Fail(String),
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum ConfinedCaseOutcome {
    Passed,
    Skipped(&'static str),
}

/// Mirrors the Linux probe path at `crates/pi-sandbox/src/backend.rs:90-94`
/// and its absence result at `crates/pi-sandbox/src/launcher.rs:204-211`.
/// Only the explicit unavailable error or an executable-not-found error naming
/// a system bwrap path is absence; every other launcher error is a broken tool.
pub(crate) fn classify_confined_launcher_gate(
    is_forced_unsupported: bool,
    bootstrap_error: Option<&str>,
) -> ConfinedLauncherGate {
    if is_forced_unsupported {
        return ConfinedLauncherGate::Skip(FORCED_UNSUPPORTED);
    }
    let Some(message) = bootstrap_error else {
        return ConfinedLauncherGate::Run;
    };
    let lower = message.to_ascii_lowercase();
    let is_explicitly_unavailable = lower.contains("trusted bubblewrap launcher is unavailable");
    let names_system_bwrap = lower.contains("/usr/bin/bwrap") || lower.contains("/bin/bwrap");
    let is_executable_not_found = lower.contains("no such file") || lower.contains("not found");
    if is_explicitly_unavailable || (names_system_bwrap && is_executable_not_found) {
        return ConfinedLauncherGate::Skip(BUBBLEWRAP_NOT_INSTALLED);
    }
    ConfinedLauncherGate::Fail(message.to_owned())
}

type ConfinedFixture = (
    TempDir,
    Application,
    FauxProviderRegistration,
    ResourceManager,
);

/// Confined-mode bootstrap behind the trusted-launcher gate.
async fn confined_bootstrap(agent_settings: Option<&str>) -> Result<ConfinedFixture, &'static str> {
    let (root, session, registration, resources) = fixture(agent_settings);
    session
        .attach_resources(resources.clone())
        .await
        .expect("attach resources");
    match Application::from_runtime_candidate(ApplicationRuntimeCandidate::new(session)).await {
        Ok(application) => Ok((root, application, registration, resources)),
        Err(error) => {
            let message = format!("{error:#}");
            match classify_confined_launcher_gate(false, Some(&message)) {
                ConfinedLauncherGate::Skip(reason) => Err(reason),
                ConfinedLauncherGate::Fail(message) => {
                    panic!("confined bootstrap failed with a present or broken launcher: {message}")
                }
                ConfinedLauncherGate::Run => {
                    panic!("confined launcher gate lost the bootstrap error: {message}")
                }
            }
        }
    }
}

/// Honors the deterministic forced-unsupported seam and otherwise bootstraps
/// the live confined fixture.
async fn live_fixture(agent_settings: &str) -> Result<ConfinedFixture, &'static str> {
    match classify_confined_launcher_gate(
        std::env::var_os("PI_TEST_SANDBOX_FORCE_UNSUPPORTED").is_some(),
        None,
    ) {
        ConfinedLauncherGate::Skip(reason) => Err(reason),
        ConfinedLauncherGate::Run => confined_bootstrap(Some(agent_settings)).await,
        ConfinedLauncherGate::Fail(message) => {
            panic!("forced confined launcher gate failed unexpectedly: {message}")
        }
    }
}

fn tool(application: &Application, name: &str) -> AgentTool {
    application
        .get_all_tools()
        .into_iter()
        .find(|tool| tool.name == name)
        .unwrap_or_else(|| panic!("application has no {name} tool"))
}

fn make_ctx(arguments: Value) -> ToolCallContext {
    let (controller, abort) = AbortController::new();
    std::mem::forget(controller);
    let on_update: ToolUpdateFn = Arc::new(|_result: AgentToolResult| {});
    ToolCallContext {
        tool_call_id: "sandbox-smoke".to_owned(),
        arguments,
        on_update,
        abort,
        model: None,
        approval_token: None,
    }
}

fn result_text(result: &AgentToolResult) -> String {
    match result.content.first() {
        Some(ContentBlock::Text { text, .. }) => text.clone(),
        _ => String::new(),
    }
}

/// Runs a tool call and returns the merged error message.
async fn call_err(tool: &AgentTool, arguments: Value) -> String {
    (tool.execute)(make_ctx(arguments))
        .await
        .expect_err("expected tool call to fail")
        .to_string()
}

/// A path is denied when ANY fail-closed layer rejects it. Different tools
/// stop a workspace-escape at different layers (the scoped-path resolver says
/// "escapes working directory"; the policy filesystem says "capability does
/// not cover" / "denied"; a confined shell says "No such file"), so the deny
/// proof accepts every one of those indicators.
fn is_denied(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("escape")
        || lower.contains("working directory")
        || lower.contains("outside")
        || lower.contains("capability")
        || lower.contains("denied")
        || lower.contains("not found")
        || lower.contains("read-only")
        || lower.contains("no such file")
}

// ---------------------------------------------------------------------------
// Deterministic tier (danger mode — no launcher probe, every host)
// ---------------------------------------------------------------------------

/// The governed Application writes the workspace through the generation-bound
/// `write` tool, reads it back through `read`, and executes shell writes
/// through the generation-bound `bash` tool — the restored workspace-write
/// boundary of the deleted file, on the current Application/generation API.

pub(crate) async fn governed_application_writes_and_reads_the_workspace() {
    let (root, application, _registration, _resources) = bootstrap(Some(DANGER_SETTINGS)).await;
    let cwd = root.path().join("cwd");
    let write = tool(&application, "write");
    let read = tool(&application, "read");
    let bash = tool(&application, "bash");

    let written = (write.execute)(make_ctx(json!({
        "path": "smoke-write-test",
        "content": "wrote",
    })))
    .await
    .expect("generation-bound write");
    assert!(
        result_text(&written).contains("wrote"),
        "write result missing content: {:?}",
        written.content
    );
    assert!(
        cwd.join("smoke-write-test").is_file(),
        "write must land in the host workspace"
    );

    let read_back = (read.execute)(make_ctx(json!({ "path": "smoke-write-test" })))
        .await
        .expect("generation-bound read");
    assert!(
        result_text(&read_back).contains("wrote"),
        "read must return the written content: {}",
        result_text(&read_back)
    );

    let bash_write = (bash.execute)(make_ctx(json!({
        "command": "printf probe > smoke-bash-test && cat smoke-bash-test",
    })))
    .await
    .expect("generation-bound bash");
    assert!(
        result_text(&bash_write).contains("probe"),
        "bash must execute and write: {}",
        result_text(&bash_write)
    );
    assert!(
        cwd.join("smoke-bash-test").is_file(),
        "bash write must land in the host workspace"
    );
    std::fs::remove_file(cwd.join("smoke-write-test")).expect("cleanup");
    std::fs::remove_file(cwd.join("smoke-bash-test")).expect("cleanup");
}

/// Generation-bound tools carry the mode's filesystem authority through the
/// captured `PolicyFs`. In `danger-full-access` (the only mode the deterministic
/// tier can bootstrap without a trusted launcher) the documented contract is
/// genuinely ambient for the content tools: `/etc` reads and out-of-workspace
/// writes are granted, not narrowed to the workspace root. This proves danger
/// mode is NOT accidentally cwd-scoped — silently confining it would break the
/// documented ambient contract. The directory `ls` tool is deliberately
/// workspace-scoped in every mode (its path resolver enforces root containment
/// unconditionally). The deny-by-default deny contract for content tools belongs
/// to confined `workspace-write` mode, which requires the trusted launcher and
/// is asserted by the live-tier
/// `confined_workspace_write_writes_workspace_and_denies_outside` test.
pub(crate) async fn generation_bound_tools_are_ambient_in_danger_mode() {
    let (_root, application, _registration, _resources) = bootstrap(Some(DANGER_SETTINGS)).await;
    let read = tool(&application, "read");
    let write = tool(&application, "write");
    let ls = tool(&application, "ls");

    // Ambient read: /etc/passwd is readable through the generation-bound read
    // tool and returns its host content.
    let read_result = (read.execute)(make_ctx(json!({ "path": "/etc/passwd" })))
        .await
        .expect("danger mode grants ambient read");
    assert!(
        result_text(&read_result).contains("root:"),
        "danger mode must read the real /etc/passwd: {}",
        result_text(&read_result)
    );

    // Ambient write: an out-of-workspace (host) write lands, then is removed.
    let outside =
        std::env::temp_dir().join(format!("pi-sandbox-smoke-outside-{}", uuid::Uuid::now_v7()));
    let write_result = (write.execute)(make_ctx(json!({
        "path": outside.to_string_lossy(),
        "content": "ambient",
    })))
    .await
    .expect("danger mode grants ambient write");
    assert!(
        result_text(&write_result).contains("Successfully wrote"),
        "ambient write result: {:?}",
        write_result.content
    );
    assert!(
        outside.is_file(),
        "danger mode write must land on the host ({})",
        outside.display()
    );
    std::fs::remove_file(&outside).expect("cleanup ambient write");

    // ls is workspace-scoped in every mode: /etc escapes the workspace roots
    // even under danger-full-access.
    let ls_error = call_err(&ls, json!({ "path": "/etc" })).await;
    assert!(
        is_denied(&ls_error),
        "ls must stay workspace-scoped even in danger mode: {ls_error}"
    );
}

/// Hostile RELATIVE path values in settings are typed configuration
/// rejections: the settings layer requires absolute policy paths, so a
/// `$(...)`-laden relative value is rejected before any probe or execution
/// and the payload never runs.
pub(crate) async fn hostile_relative_path_values_are_typed_rejections_and_never_executed() {
    let sentinel = std::env::temp_dir().join(format!(
        "pi-sandbox-eval-pwned-relative-{}",
        uuid::Uuid::now_v7()
    ));
    let hostile = format!("$(touch {}) 'quoted'", sentinel.display());
    let settings = json!({
        "sandbox": {
            "mode": "workspace-write",
            "deniedPaths": [hostile],
        }
    })
    .to_string();
    let (_root, _registration, _resources, message) = bootstrap_err(Some(&settings)).await;
    assert!(
        message.contains("absolute"),
        "a relative hostile path must be rejected as not absolute: {message}"
    );
    assert!(
        !sentinel.exists(),
        "hostile path values must be handled as literals and never executed"
    );
}

/// Hostile ABSOLUTE path values are literal policy strings: they pass path
/// validation unchanged (they are never evaluated), and the mode conflict
/// check for danger mode sees the literal string — the payload never runs.
pub(crate) async fn hostile_absolute_path_values_are_literal_strings_never_evaluated() {
    let sentinel = std::env::temp_dir().join(format!(
        "pi-sandbox-eval-pwned-absolute-{}",
        uuid::Uuid::now_v7()
    ));
    let hostile = format!("/tmp/$(touch {}) 'quoted'", sentinel.display());

    // The path layer treats the payload as a literal absolute path.
    let parsed = AbsolutePath::parse(PathBuf::from(&hostile)).expect("literal absolute path");
    assert_eq!(parsed.as_path().to_string_lossy(), hostile);
    // The relative payload is not an absolute path, so it is rejected.
    assert!(
        AbsolutePath::parse(PathBuf::from(format!(
            "$(touch {}) 'quoted'",
            sentinel.display()
        )))
        .is_err()
    );

    // In danger mode the literal denied path conflicts with the mode — the
    // validation sees the string itself, never its contents, and the payload
    // cannot execute during bootstrap.
    let settings = json!({
        "sandbox": {
            "mode": "danger-full-access",
            "deniedPaths": [hostile],
        }
    })
    .to_string();
    let (_root, _registration, _resources, message) = bootstrap_err(Some(&settings)).await;
    assert!(
        message.contains("conflicting") && message.contains("deniedPaths"),
        "danger mode must reject the literal denied path as a conflict: {message}"
    );
    assert!(
        !sentinel.exists(),
        "hostile path values must be handled as literals and never executed"
    );
}

/// Settings-mode precedence is deterministic: omission resolves to raw-host
/// full access, and the wire rejects unknown sandbox fields instead of
/// silently ignoring a typo that would weaken the sandbox.
pub(crate) fn sandbox_settings_resolution_and_wire_precedence_are_deterministic() {
    assert_eq!(
        resolved_sandbox_mode(None),
        SandboxMode::DangerFullAccess,
        "omitting sandbox settings must resolve to danger-full-access"
    );
    let empty = serde_json::from_str::<SandboxSettingsWire>(r#"{}"#).expect("empty wire");
    assert_eq!(
        resolved_sandbox_mode(Some(&empty)),
        SandboxMode::DangerFullAccess
    );
    for (wire, expected) in [
        (r#"{"mode":"read-only"}"#, SandboxMode::ReadOnly),
        (r#"{"mode":"workspace-write"}"#, SandboxMode::WorkspaceWrite),
        (
            r#"{"mode":"danger-full-access"}"#,
            SandboxMode::DangerFullAccess,
        ),
    ] {
        let settings = serde_json::from_str::<SandboxSettingsWire>(wire)
            .unwrap_or_else(|error| panic!("valid wire {wire} failed: {error}"));
        assert_eq!(
            resolved_sandbox_mode(Some(&settings)),
            expected,
            "mode resolution for {wire}"
        );
    }
    let unknown = serde_json::from_str::<SandboxSettingsWire>(
        r#"{"mode":"danger-full-access","bogusField":1}"#,
    );
    assert!(
        unknown.is_err(),
        "an unknown sandbox settings field must be rejected, never silently ignored"
    );
}

// ---------------------------------------------------------------------------
// Live confinement tier (harnessless target with trusted bubblewrap launcher)
// ---------------------------------------------------------------------------

/// Confined workspace-write: the workspace is writable through the
/// generation-bound tools and the governed bash PTY path, while everything
/// outside the workspace (host `/etc`, absolute writes) is denied.
pub(crate) async fn confined_workspace_write_writes_workspace_and_denies_outside()
-> ConfinedCaseOutcome {
    let (root, application, _registration, _resources) =
        match live_fixture(WORKSPACE_WRITE_SETTINGS).await {
            Ok(fixture) => fixture,
            Err(reason) => return ConfinedCaseOutcome::Skipped(reason),
        };
    let cwd = root.path().join("cwd");
    let write = tool(&application, "write");
    let read = tool(&application, "read");
    let bash = tool(&application, "bash");

    // The workspace is writable through the generation-bound write tool and
    // lands on the host (bind-mounted writable root).
    let written = (write.execute)(make_ctx(json!({
        "path": "confined-ws-probe",
        "content": "probe",
    })))
    .await
    .expect("confined workspace write");
    assert!(
        result_text(&written).contains("probe"),
        "confined workspace write failed: {:?}",
        written.content
    );
    assert!(
        cwd.join("confined-ws-probe").is_file(),
        "confined write must land in the host workspace"
    );

    // The governed bash PTY path also writes the workspace.
    let bash_write = (bash.execute)(make_ctx(json!({
        "command": "printf pty > confined-pty-probe && cat confined-pty-probe",
        "pty": true,
    })))
    .await
    .expect("confined pty bash workspace write");
    assert!(
        result_text(&bash_write).contains("pty"),
        "confined pty bash write failed: {}",
        result_text(&bash_write)
    );
    assert!(
        cwd.join("confined-pty-probe").is_file(),
        "pty write must land in the host workspace"
    );

    // Runtime files required to execute ordinary host tools remain readable;
    // non-runtime host paths stay outside the confined policy roots.
    let runtime_read = (read.execute)(make_ctx(json!({ "path": "/etc/passwd" })))
        .await
        .expect("confined runtime file must remain readable");
    assert!(
        result_text(&runtime_read).contains("root:"),
        "confined runtime read must return passwd content: {}",
        result_text(&runtime_read)
    );
    let denied_read = call_err(&read, json!({ "path": "/etc/shadow" })).await;
    assert!(
        is_denied(&denied_read),
        "confined read outside runtime roots must fail closed: {denied_read}"
    );
    let bash_read = (bash.execute)(make_ctx(json!({
        "command": "cat /etc/shadow",
        "pty": true,
    })))
    .await
    .expect_err("confined pty bash must not read /etc/shadow")
    .to_string();
    assert!(
        bash_read.contains("No such file"),
        "expected /etc/shadow invisible to the confined shell: {bash_read}"
    );

    // Absolute writes outside the workspace fail closed and never land.
    let outside = std::env::temp_dir().join(format!(
        "pi-sandbox-confined-outside-{}",
        uuid::Uuid::now_v7()
    ));
    let write_error = call_err(
        &write,
        json!({ "path": outside.to_string_lossy(), "content": "pwned" }),
    )
    .await;
    assert!(!outside.exists(), "confined write must never land outside");
    assert!(
        is_denied(&write_error),
        "confined outside write must fail closed: {write_error}"
    );
    ConfinedCaseOutcome::Passed
}

/// Confined read-only mode: reads of the workspace keep working while every
/// mutation — generation-bound write/edit tools and governed shell writes —
/// fails closed and nothing lands on the host.
pub(crate) async fn confined_read_only_mode_allows_reads_and_blocks_writes() -> ConfinedCaseOutcome
{
    let (root, application, _registration, _resources) =
        match live_fixture(READ_ONLY_SETTINGS).await {
            Ok(fixture) => fixture,
            Err(reason) => return ConfinedCaseOutcome::Skipped(reason),
        };
    let cwd = root.path().join("cwd");
    std::fs::write(cwd.join("allowed.txt"), "open").expect("allowed file");
    let read = tool(&application, "read");
    let write = tool(&application, "write");
    let edit = tool(&application, "edit");
    let bash = tool(&application, "bash");

    let read_ok = (read.execute)(make_ctx(json!({ "path": "allowed.txt" })))
        .await
        .expect("read-only reads must work");
    assert!(
        result_text(&read_ok).contains("open"),
        "read-only read must return content: {}",
        result_text(&read_ok)
    );

    let write_error = call_err(&write, json!({ "path": "ro-write-probe", "content": "x" })).await;
    assert!(
        write_error.contains("read-only"),
        "write tool must fail with a read-only reason: {write_error}"
    );
    assert!(
        !cwd.join("ro-write-probe").exists(),
        "no file may land on the host in read-only mode"
    );

    let edit_error = call_err(
        &edit,
        json!({
            "path": "allowed.txt",
            "edits": [{ "oldText": "open", "newText": "closed" }],
        }),
    )
    .await;
    assert!(
        edit_error.contains("read-only"),
        "edit tool must fail with a read-only reason: {edit_error}"
    );

    let bash_write = (bash.execute)(make_ctx(json!({
        "command": "for fd in /proc/self/fd/*; do if [ \"$(readlink \"$fd\")\" = \"$PWD\" ]; then printf x > \"$fd/ro-fd-probe\" && echo cwd-fd-writable; fi; done; printf x > ro-shell-probe",
        "pty": true,
    })))
    .await
    .expect_err("read-only shell write must fail")
    .to_string();
    assert!(
        bash_write.contains("Read-only file system") || bash_write.contains("read-only"),
        "shell write must fail on the read-only mount: {bash_write}"
    );
    assert!(
        !bash_write.contains("cwd-fd-writable"),
        "no inherited cwd descriptor may bypass read-only authority: {bash_write}"
    );
    assert!(
        !cwd.join("ro-shell-probe").exists() && !cwd.join("ro-fd-probe").exists(),
        "no path or descriptor write may land on the host in read-only mode"
    );
    ConfinedCaseOutcome::Passed
}

/// Confined governed subprocesses get the private environment projection:
/// HOME and TMPDIR point into the sandbox root (`/run/pi/home`, `/tmp`), the
/// private TMPDIR is writable, and the host HOME never leaks into the
/// confined environment.
pub(crate) async fn confined_bash_pty_redirects_home_and_tmpdir_to_private_projection()
-> ConfinedCaseOutcome {
    let (_root, application, _registration, _resources) =
        match live_fixture(WORKSPACE_WRITE_SETTINGS).await {
            Ok(fixture) => fixture,
            Err(reason) => return ConfinedCaseOutcome::Skipped(reason),
        };
    let host_home = std::env::var("HOME").unwrap_or_default();
    let bash = tool(&application, "bash");

    let probe = (bash.execute)(make_ctx(json!({
        "command": "printf 'HOME=%s\\nTMPDIR=%s\\n' \"$HOME\" \"$TMPDIR\" && touch \"$TMPDIR/pi-tmp-probe\" && echo tmp-writable && ls \"$HOME\" >/dev/null 2>&1 && echo home-writable",
        "pty": true,
    })))
    .await
    .expect("confined HOME/TMPDIR probe");
    let text = result_text(&probe);
    assert!(
        text.contains("HOME=/run/pi/home"),
        "HOME must be redirected to the sandbox home: {text}"
    );
    assert!(
        text.contains("TMPDIR=/tmp"),
        "TMPDIR must point at the private tmpfs: {text}"
    );
    assert!(
        text.contains("tmp-writable"),
        "TMPDIR must be writable: {text}"
    );
    assert!(
        text.contains("home-writable"),
        "the sandbox HOME must be readable: {text}"
    );
    if !host_home.is_empty() && host_home != "/run/pi/home" {
        assert!(
            !text.contains(&host_home),
            "the host HOME must not leak into the confined environment: {text}"
        );
    }
    ConfinedCaseOutcome::Passed
}

/// Confined child networking is denied by both the fixed seccomp policy and
/// an isolated network namespace. A non-loopback connect must fail without
/// reaching the host network.
pub(crate) async fn confined_bash_pty_network_is_off_by_default() -> ConfinedCaseOutcome {
    let (_root, application, _registration, _resources) =
        match live_fixture(WORKSPACE_WRITE_SETTINGS).await {
            Ok(fixture) => fixture,
            Err(reason) => return ConfinedCaseOutcome::Skipped(reason),
        };
    let bash = tool(&application, "bash");
    let connect = (bash.execute)(make_ctx(json!({
        "command": "(echo > /dev/tcp/93.184.216.34/80)",
        "pty": true,
        "timeout": 10,
    })))
    .await
    .expect_err("non-loopback connect must fail inside the confined shell")
    .to_string();
    assert!(
        connect.contains("Operation not permitted") || connect.contains("Network is unreachable"),
        "expected network-off failure: {connect}"
    );
    ConfinedCaseOutcome::Passed
}

/// A hostile absolute denied path is treated as a literal policy value. The
/// missing literal path fails closed during spawn preparation, and the
/// embedded shell payload is never evaluated.
pub(crate) async fn confined_hostile_absolute_denied_path_is_literal_and_never_evaluated()
-> ConfinedCaseOutcome {
    let sentinel = std::env::temp_dir().join(format!(
        "pi-sandbox-eval-pwned-confined-{}",
        uuid::Uuid::now_v7()
    ));
    let hostile = format!("/tmp/$(touch {}) 'quoted'", sentinel.display());
    let settings = json!({
        "sandbox": {
            "mode": "workspace-write",
            "deniedPaths": [hostile],
        }
    })
    .to_string();
    let (_root, application, _registration, _resources) = match live_fixture(&settings).await {
        Ok(fixture) => fixture,
        Err(reason) => return ConfinedCaseOutcome::Skipped(reason),
    };
    let bash = tool(&application, "bash");
    let error = (bash.execute)(make_ctx(json!({
        "command": "printf 'must-not-run'",
        "pty": true,
    })))
    .await
    .expect_err("missing literal denied path must fail closed")
    .to_string();
    assert!(
        error.contains("required denied path is unavailable"),
        "literal denied path failure expected: {error}"
    );
    assert!(
        !sentinel.exists(),
        "hostile path values must be handled as literals and never executed"
    );
    ConfinedCaseOutcome::Passed
}

/// Settings precedence at the generation level: a settings reload swaps the
/// published generation and the new mode governs the very next tool call —
/// workspace-write → read-only blocks writes while reads keep working, and a
/// reload back to workspace-write restores writes. Each successful reload
/// advances the published revision.
pub(crate) async fn confined_settings_reload_swaps_generation_mode_and_governs_tools()
-> ConfinedCaseOutcome {
    let (root, application, _registration, _resources) =
        match live_fixture(WORKSPACE_WRITE_SETTINGS).await {
            Ok(fixture) => fixture,
            Err(reason) => return ConfinedCaseOutcome::Skipped(reason),
        };
    let agent_dir = root.path().join("agent-home");
    let cwd = root.path().join("cwd");
    let write = tool(&application, "write");
    let read = tool(&application, "read");
    assert_eq!(
        application.resource_generation(),
        Some(1),
        "bootstrap publishes revision one"
    );

    // Workspace-write: writes work.
    let written = (write.execute)(make_ctx(json!({
        "path": "reload-write-probe",
        "content": "ok",
    })))
    .await
    .expect("workspace-write generation must allow writes");
    assert_eq!(
        std::fs::read_to_string(cwd.join("reload-write-probe")).expect("workspace write"),
        "ok"
    );

    // Reload to read-only: the new generation governs the next call.
    std::fs::write(agent_dir.join("settings.json"), READ_ONLY_SETTINGS)
        .expect("read-only settings");
    let reloaded = application.reload().await.expect("reload to read-only");
    assert_eq!(reloaded.generation, 2, "reload must advance the revision");
    assert_eq!(application.resource_generation(), Some(2));

    let denied = call_err(
        &write,
        json!({ "path": "reload-write-probe-2", "content": "x" }),
    )
    .await;
    assert!(
        denied.contains("read-only"),
        "read-only generation must block writes: {denied}"
    );
    assert!(
        !cwd.join("reload-write-probe-2").exists(),
        "no write may land under the read-only generation"
    );
    let read_ok = (read.execute)(make_ctx(json!({ "path": "reload-write-probe" })))
        .await
        .expect("reads must keep working after the read-only reload");
    assert!(
        result_text(&read_ok).contains("ok"),
        "{}",
        result_text(&read_ok)
    );

    // Reload back to workspace-write: writes are governed again.
    std::fs::write(agent_dir.join("settings.json"), WORKSPACE_WRITE_SETTINGS)
        .expect("workspace-write settings");
    let restored = application
        .reload()
        .await
        .expect("reload to workspace-write");
    assert_eq!(
        restored.generation, 3,
        "the reload origin must bind the captured base"
    );
    assert_eq!(application.resource_generation(), Some(3));
    let rewritten = (write.execute)(make_ctx(json!({
        "path": "reload-write-probe-3",
        "content": "restored",
    })))
    .await
    .expect("workspace-write generation must allow writes again");
    assert_eq!(
        std::fs::read_to_string(cwd.join("reload-write-probe-3"))
            .expect("restored workspace write"),
        "restored"
    );
    ConfinedCaseOutcome::Passed
}

/// A timed-out governed bash command is reaped: the process-tree
/// termination covers the confined child, so no orphan `sleep` survives.
pub(crate) async fn confined_bash_timeout_reaps_descendants() -> ConfinedCaseOutcome {
    let (_root, application, _registration, _resources) =
        match live_fixture(WORKSPACE_WRITE_SETTINGS).await {
            Ok(fixture) => fixture,
            Err(reason) => return ConfinedCaseOutcome::Skipped(reason),
        };
    let bash = tool(&application, "bash");
    let error = (bash.execute)(make_ctx(json!({
        "command": "sleep 300.7",
        "pty": true,
        "timeout": 0.5,
    })))
    .await
    .expect_err("confined sleep must time out")
    .to_string();
    assert!(
        error.contains("Command execution timed out after"),
        "timeout error expected: {error}"
    );
    // Give the process-tree termination a moment, then confirm no confined
    // sleep remains (bounded retry so a slow reaper still settles).
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let mut leftover = Vec::new();
    while std::time::Instant::now() < deadline {
        let probe = std::process::Command::new("pgrep")
            .args(["-f", "sleep 300.7"])
            .output()
            .expect("pgrep");
        leftover = probe.stdout.clone();
        if !probe.status.success() || leftover.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(
        leftover.is_empty(),
        "confined sleep must not survive the timeout: {:?}",
        String::from_utf8_lossy(&leftover)
    );
    ConfinedCaseOutcome::Passed
}

/// Confined non-PTY commands run in a real governed Bash subprocess: the
/// sandbox baseline wins over ambient HOME/TMPDIR, ordinary pager variables
/// remain available, and a readable host-only file outside the workspace is
/// invisible inside the child.
pub(crate) async fn confined_bash_foreground_uses_governed_subprocess_authority()
-> ConfinedCaseOutcome {
    let (root, application, _registration, _resources) =
        match live_fixture(WORKSPACE_WRITE_SETTINGS).await {
            Ok(fixture) => fixture,
            Err(reason) => return ConfinedCaseOutcome::Skipped(reason),
        };
    let outside = root.path().join("outside-host-readable");
    std::fs::write(&outside, "host-only-marker").expect("host-only readable file");
    let outside = outside.to_string_lossy();
    assert!(
        !outside.contains('\''),
        "fixture path must be shell safe: {outside}"
    );
    let bash = tool(&application, "bash");
    let command = format!(
        "printf 'BASH=%s\\nHOME=%s\\nTMPDIR=%s\\nPAGER=%s\\n' \\
         \"${{BASH_VERSION:+yes}}\" \"$HOME\" \"$TMPDIR\" \"$PAGER\"; cat '{outside}'"
    );
    let error = (bash.execute)(make_ctx(json!({
        "command": command,
        "timeout": 10,
    })))
    .await
    .expect_err("confined non-PTY bash must not read a host-only file")
    .to_string();
    assert!(
        error.contains("BASH=yes"),
        "real Bash subprocess expected: {error}"
    );
    assert!(
        error.contains("HOME=/run/pi/home"),
        "HOME must stay the confined baseline, not a host override: {error}"
    );
    assert!(
        error.contains("TMPDIR=/tmp"),
        "TMPDIR must stay the confined baseline: {error}"
    );
    assert!(
        error.contains("PAGER=cat"),
        "ordinary unattended pager must still be requested: {error}"
    );
    assert!(
        error.contains("No such file") && !error.contains("host-only-marker"),
        "host-only file must be invisible to confined non-PTY bash: {error}"
    );
    ConfinedCaseOutcome::Passed
}

/// Confined tool-level bash commands never queue on the session-tree brush
/// execution lock: the second command starts and completes with only its own
/// bounded output while the first command is still gated open. (This is the
/// harnessless home of the former libtest confined bypass tests: governed
/// confined subprocesses re-execute the current binary as the hidden child,
/// so they can only be proven from this `__sandbox-child`-dispatching
/// harness, never from the libtest harness.)
pub(crate) async fn confined_bash_tool_bypasses_brush_lock_serialization()
-> ConfinedCaseOutcome {
    let (root, application, _registration, _resources) =
        match live_fixture(WORKSPACE_WRITE_SETTINGS).await {
            Ok(fixture) => fixture,
            Err(reason) => return ConfinedCaseOutcome::Skipped(reason),
        };
    let cwd = root.path().join("cwd");
    let gate = cwd.join("brush-bypass-gate");
    let bash = tool(&application, "bash");
    let first = tokio::spawn({
        let bash = bash.clone();
        let gate = gate.clone();
        async move {
            (bash.execute)(make_ctx(json!({
                "command": format!(
                    "printf started > c1.start; while [ ! -s {} ]; do sleep 0.05; done; printf done > c1.end",
                    gate.display()
                ),
                "timeout": 60,
            })))
            .await
            .expect("first confined command completes")
        }
    });
    // Wait until the first command has crossed its start boundary and holds
    // its run (gate still closed) before launching the second command, so
    // the second's progress can only come from bypassing the serialization
    // the first command holds.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !cwd.join("c1.start").exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "first confined command never crossed its start boundary"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        !gate.exists(),
        "the gate must still be closed when the second command launches"
    );
    let second_spawned = std::time::Instant::now();
    let second = tokio::spawn({
        let bash = bash.clone();
        async move {
            (bash.execute)(make_ctx(json!({
                "command": "echo ok",
                "timeout": 30,
            })))
            .await
            .expect("second confined command completes")
        }
    });
    // The second command must complete promptly — and therefore entirely
    // while the gate is still closed — even though the first command holds
    // its start boundary: confined runs never queue on the brush exec lock.
    let second_result = tokio::time::timeout(Duration::from_secs(20), second)
        .await
        .expect("second confined command must complete while the gate is held: \
                 confined runs must bypass the brush execution lock")
        .expect("second task joins");
    assert!(
        second_spawned.elapsed() < Duration::from_secs(20),
        "second confined command must finish promptly: {:?}",
        second_spawned.elapsed()
    );
    assert!(
        !gate.exists(),
        "the gate must remain closed across the second command's whole run"
    );
    // Confined output carries only the command's own bounded output.
    let second_text = match second_result.content.first() {
        Some(ContentBlock::Text { text, .. }) => text.clone(),
        other => panic!("second confined command must return text: {other:?}"),
    };
    assert_eq!(
        second_text, "ok\n",
        "confined output must be only the command's own output: {second_text:?}"
    );
    std::fs::write(&gate, b"release").expect("release the first command");
    first.await.expect("first task joins");
    ConfinedCaseOutcome::Passed
}

/// Timeout on the confined non-PTY fallback reaps the child and keeps the
/// primary timeout outcome (no hang, no reader I/O replacement).
pub(crate) async fn confined_bash_foreground_fallback_timeout_reaps_descendants()
-> ConfinedCaseOutcome {
    let (_root, application, _registration, _resources) =
        match live_fixture(WORKSPACE_WRITE_SETTINGS).await {
            Ok(fixture) => fixture,
            Err(reason) => return ConfinedCaseOutcome::Skipped(reason),
        };
    let bash = tool(&application, "bash");
    let started = std::time::Instant::now();
    let error = (bash.execute)(make_ctx(json!({
        "command": "sleep 300.8 |& cat",
        "timeout": 0.5,
    })))
    .await
    .expect_err("confined fallback sleep must time out")
    .to_string();
    assert!(
        error.contains("Command execution timed out after"),
        "timeout error expected: {error}"
    );
    assert!(
        !error.contains("output reader failed"),
        "timeout must not be replaced by a reader I/O error: {error}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "confined fallback timeout cleanup must not hang: {:?}",
        started.elapsed()
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let mut leftover = Vec::new();
    while std::time::Instant::now() < deadline {
        let probe = std::process::Command::new("pgrep")
            .args(["-f", "sleep 300.8"])
            .output()
            .expect("pgrep");
        leftover = probe.stdout.clone();
        if !probe.status.success() || leftover.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(
        leftover.is_empty(),
        "confined fallback sleep must not survive the timeout: {:?}",
        String::from_utf8_lossy(&leftover)
    );
    ConfinedCaseOutcome::Passed
}

/// A governed subagent child inherits the parent Application's generation:
/// its generation-bound tools are confined exactly like the parent's — the
/// child cannot read host paths and writes through its own tools stay inside
/// the shared workspace root.
pub(crate) async fn governed_subagent_child_inherits_parent_generation_and_stays_confined()
-> ConfinedCaseOutcome {
    let (root, application, _registration, _resources) =
        match live_fixture(WORKSPACE_WRITE_SETTINGS).await {
            Ok(fixture) => fixture,
            Err(reason) => return ConfinedCaseOutcome::Skipped(reason),
        };
    let cwd = root.path().join("cwd");

    let parent_session = application.session();
    let factory = pi_coding::OrchestrationRuntime::child_factory_from_session(&parent_session);
    let definition = pi_coding::parse_agent_definition(
        Path::new("task.md"),
        "---\nname: task\ndescription: task\n---\nprompt",
        AgentDefinitionSource::Bundled,
        true,
    )
    .expect("definition");
    let request = pi_coding::ChildSessionRequest {
        child_id: "SandboxChild".to_owned(),
        parent_id: "Main".to_owned(),
        max_tools_per_agent: 32,
        depth: 1,
        definition,
        assignment: "probe the sandbox".to_owned(),
        system_prompt: "probe".to_owned(),
        requested_tool_names: None,
        persona_skill_store: None,
        persona_skills: Arc::new(Vec::new()),
        shared_skills: Arc::new(Vec::new()),
        orchestration_tools: Vec::new(),
        thinking_level: None,
        model: parent_session.model().expect("parent model"),
        resolved_model_sequence: Vec::new(),
        resolved_model_selected_index: 0,
        output_schema: None,
        schema_mode: None,
        yield_state: Arc::new(pi_coding::YieldState::default()),
    };
    let child = (factory)(request).await.expect("child session");
    let child_bash = child
        .get_all_tools()
        .into_iter()
        .find(|tool| tool.name == "bash")
        .expect("child bash tool");
    let child_write = child
        .get_all_tools()
        .into_iter()
        .find(|tool| tool.name == "write")
        .expect("child write tool");

    // The child's governed shell is confined by the inherited generation:
    // non-runtime host paths are invisible.
    let denied = (child_bash.execute)(make_ctx(json!({
        "command": "cat /etc/shadow",
        "pty": true,
    })))
    .await
    .expect_err("sandboxed child must not read /etc/shadow")
    .to_string();
    assert!(
        denied.contains("No such file"),
        "expected /etc/shadow invisible to the governed child: {denied}"
    );

    // The child's generation-bound write tool writes into the shared
    // workspace root and the file lands on the host.
    let written = (child_write.execute)(make_ctx(json!({
        "path": "child-workspace-probe",
        "content": "probe",
    })))
    .await
    .expect("governed child writes its workspace");
    assert!(
        result_text(&written).contains("probe"),
        "child workspace write failed: {:?}",
        written.content
    );
    assert!(
        cwd.join("child-workspace-probe").is_file(),
        "the child write must land in the host workspace"
    );
    std::fs::remove_file(cwd.join("child-workspace-probe")).expect("cleanup");
    ConfinedCaseOutcome::Passed
}
