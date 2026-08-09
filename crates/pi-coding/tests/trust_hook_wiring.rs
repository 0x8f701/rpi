//! End-to-end wiring test for the T91 trust hook: the production trust path
//! (`Application::reload` and the Application startup path) resolves the
//! tentative trust decision through the fail-open hook surfaces — the
//! `pre_trust_decision` host hook and the `trust_decision` extension event —
//! and composes their outcomes via `apply_trust_hook_outcomes` before the
//! `project_trusted` result is recorded in the resource snapshot.
//!
//! These tests drive the real production entry points (an `Application`
//! bound to a session with attached resources, host hooks configured through
//! `settings.hooks` exactly as production reads them, and a loaded QuickJS
//! extension listening on `trust_decision`) — they prove the wiring, not
//! just the pieces.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use pi_coding::{
    Application, ExtensionCapability, ExtensionMode, ExtensionOrigin, ExtensionPermissionSet,
    ExtensionRuntime, ExtensionRuntimeOptions, ExtensionSpec, ExtensionSpecRuntime, ResourceManager,
    ResourceManagerOptions, Session, SessionOptions, TrustDecision, TrustStore,
};
use serde_json::{Value, json};
use tempfile::TempDir;
use uuid::Uuid;

/// Fixture project with trust-gated `.pi` resources, an isolated agent dir
/// (empty trust store), and an executable host-hook script that captures the
/// `pre_trust_decision` payload (one JSON object per line) before answering
/// with the configured response. The hook is declared through
/// `settings.hooks` in the agent dir — the production configuration surface.
struct Fixture {
    _cwd: TempDir,
    _agent: TempDir,
    _hook_dir: TempDir,
    cwd: PathBuf,
    agent_dir: PathBuf,
    hook_capture: PathBuf,
}

impl Fixture {
    fn new() -> Result<Self> {
        let cwd = TempDir::new()?;
        let agent = TempDir::new()?;
        let hook_dir = TempDir::new()?;
        let cwd_path = cwd.path().canonicalize()?;
        let agent_path = agent.path().canonicalize()?;
        let hook_capture = hook_dir.path().join("payloads.jsonl");
        // Trust-gated project resources: their presence makes the trust store
        // consulted, so the hook observation fires.
        let skill = cwd_path.join(".pi").join("skills").join("secret");
        fs::create_dir_all(&skill)?;
        fs::write(
            skill.join("SKILL.md"),
            "---\nname: secret\ndescription: gated project skill\n---\n# secret\n",
        )?;
        Ok(Self {
            _cwd: cwd,
            _agent: agent,
            _hook_dir: hook_dir,
            hook_capture,
            cwd: cwd_path,
            agent_dir: agent_path,
        })
    }

    /// Resource manager for the fixture cwd with interactive trust semantics
    /// (`headless: false` keeps an undecided project at `Ask`, which is what
    /// the extension approval upgrades).
    fn resources(&self) -> ResourceManager {
        let mut options = ResourceManagerOptions::new(self.cwd.clone());
        options.agent_dir = self.agent_dir.clone();
        options.headless = false;
        ResourceManager::new(options).expect("resource manager")
    }

    /// Write an executable `pre_trust_decision` hook that appends the stdin
    /// payload to the capture file and answers with `response`, then declare
    /// it through `settings.hooks` in the agent dir.
    fn configure_hook(&self, response: &str, fail_closed: bool) -> Result<()> {
        let tmp = self._hook_dir.path().join(format!("hook.tmp-{}", Uuid::now_v7()));
        let capture = self.hook_capture.to_string_lossy();
        fs::write(
            &tmp,
            format!(
                "#!/bin/sh\nread -r payload\nprintf '%s\\n' \"$payload\" >> \"{capture}\"\necho '{response}'\n"
            ),
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(&tmp)?.permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&tmp, permissions)?;
        }
        let path = self._hook_dir.path().join("hook.sh");
        fs::rename(&tmp, &path)?;
        fs::create_dir_all(&self.agent_dir)?;
        let settings = json!({
            "hooks": [{
                "event": "pre_trust_decision",
                "command": [path.to_string_lossy()],
                "timeoutMs": 2_000,
                "failClosed": fail_closed,
            }],
        });
        fs::write(
            self.agent_dir.join("settings.json"),
            serde_json::to_vec_pretty(&settings)?,
        )?;
        Ok(())
    }

    /// Last captured `pre_trust_decision` payload, or `None` when the hook
    /// never fired.
    fn last_hook_payload(&self) -> Result<Option<Value>> {
        let content = match fs::read_to_string(&self.hook_capture) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        Ok(content
            .lines()
            .last()
            .map(|line| serde_json::from_str(line).expect("captured payload is JSON")))
    }

    fn hook_fire_count(&self) -> Result<usize> {
        let content = match fs::read_to_string(&self.hook_capture) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(error.into()),
        };
        Ok(content.lines().count())
    }
}

/// The fake QuickJS extension: listens on `trust_decision`, validates the
/// payload contract against the exact fixture path, and approves only an
/// undecided (`ask`) tentative decision. An `Ask -> Trusted` upgrade in the
/// composed outcome therefore proves the event reached this extension with
/// the observed payload; any other payload makes the handler throw (and the
/// host fails open, so the decision is never weakened).
fn extension_source(fx: &Fixture) -> String {
    let expected_path = fx.cwd.to_string_lossy();
    format!(
        r#"export default function (pi) {{
    pi.on("trust_decision", (event) => {{
        if (event.path !== "{expected_path}") {{
            throw new Error("unexpected path: " + event.path);
        }}
        if (!["trusted", "untrusted", "ask"].includes(event.decision)) {{
            throw new Error("unexpected decision: " + event.decision);
        }}
        if (typeof event.isNew !== "boolean") {{
            throw new Error("missing isNew");
        }}
        return {{ approve: event.decision === "ask" }};
    }});
}}
"#
    )
}

/// Build the session + application bound to the fixture project. The host
/// hook is already declared in the fixture's `settings.hooks`; the QuickJS
/// extension is loaded when `with_extension` is set.
async fn build_application(
    fx: &Fixture,
    with_extension: bool,
) -> Result<(Application, Option<ExtensionRuntime>)> {
    let resources = fx.resources();
    let session = Session::new(SessionOptions {
        model: pi_ai::Model::default(),
        cwd: fx.cwd.clone(),
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
    .context("building session")?;
    session
        .attach_resources(resources.clone())
        .await
        .context("attaching resources")?;

    let (application, runtime) = if with_extension {
        let entry = fx._hook_dir.path().join("trust-decision.mjs");
        fs::write(&entry, extension_source(fx))?;
        let permissions = ExtensionPermissionSet {
            capabilities: BTreeSet::from([ExtensionCapability::EventHooks]),
            ui_capabilities: BTreeSet::new(),
        };
        let spec = ExtensionSpec::new_runtime(
            "trust-decision",
            ExtensionSpecRuntime::QuickJs { entry: entry.clone() },
            fx._hook_dir.path(),
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
        let application =
            Application::new_with_extensions(session, runtime.clone(), permissions).await;
        (application, Some(runtime))
    } else {
        let application = Application::new(session).await;
        (application, None)
    };
    Ok((application, runtime))
}

#[tokio::test]
async fn reload_fires_host_hook_and_extension_and_host_block_wins() -> Result<()> {
    let fx = Fixture::new()?;
    fx.configure_hook(r#"{"decision":"block","reason":"denied by policy"}"#, true)?;
    let (application, runtime) = build_application(&fx, true).await?;

    application.reload().await.context("production reload path")?;

    let snapshot = application.resource_snapshot().expect("snapshot");
    assert_eq!(
        snapshot.trust.decision,
        TrustDecision::Untrusted,
        "host block must deny even an extension approval"
    );
    let payload = fx.last_hook_payload()?.expect("host hook must have fired");
    assert_eq!(payload["event"], "pre_trust_decision");
    assert_eq!(payload["path"], fx.cwd.to_string_lossy().as_ref());
    assert_eq!(payload["decision"], "ask");
    assert_eq!(payload["isNew"], true);
    assert_eq!(
        fx.hook_fire_count()?,
        2,
        "hook fires once at startup and once on reload"
    );

    application.cleanup().await;
    if let Some(runtime) = runtime {
        runtime.shutdown().await;
    }
    Ok(())
}

#[tokio::test]
async fn reload_extension_approval_upgrades_ask_to_trusted() -> Result<()> {
    let fx = Fixture::new()?;
    fx.configure_hook(r#"{"decision":"allow"}"#, true)?;
    let (application, runtime) = build_application(&fx, true).await?;

    application.reload().await.context("production reload path")?;

    let snapshot = application.resource_snapshot().expect("snapshot");
    assert_eq!(
        snapshot.trust.decision,
        TrustDecision::Trusted,
        "extension approval must upgrade an undecided project; the upgrade \
         proves the trust_decision event reached the extension with the exact payload"
    );
    let payload = fx.last_hook_payload()?.expect("host hook must have fired");
    assert_eq!(payload["decision"], "ask");
    assert_eq!(payload["isNew"], true);

    application.cleanup().await;
    if let Some(runtime) = runtime {
        runtime.shutdown().await;
    }
    Ok(())
}

#[tokio::test]
async fn reload_never_weakens_a_stored_denial() -> Result<()> {
    let fx = Fixture::new()?;
    fx.configure_hook(r#"{"decision":"allow"}"#, true)?;
    TrustStore::new(&fx.agent_dir)
        .set(&fx.cwd, TrustDecision::Untrusted)
        .context("storing denial")?;
    let (application, runtime) = build_application(&fx, true).await?;

    application.reload().await.context("production reload path")?;

    let snapshot = application.resource_snapshot().expect("snapshot");
    assert_eq!(
        snapshot.trust.decision,
        TrustDecision::Untrusted,
        "a stored denial must survive an extension approval"
    );
    let payload = fx.last_hook_payload()?.expect("host hook must have fired");
    assert_eq!(payload["decision"], "untrusted");
    assert_eq!(payload["isNew"], false);

    application.cleanup().await;
    if let Some(runtime) = runtime {
        runtime.shutdown().await;
    }
    Ok(())
}

#[tokio::test]
async fn reload_hook_failure_fails_open_unless_fail_closed() -> Result<()> {
    // A hook that never produces a parseable response: fail-open keeps the
    // tentative Ask; failClosed denies. No extension runtime, so the outcome
    // isolates the host-hook failure semantics.
    let fx = Fixture::new()?;
    fx.configure_hook("not json", false)?;
    let (application, runtime) = build_application(&fx, false).await?;
    application.reload().await.context("production reload path")?;
    let snapshot = application.resource_snapshot().expect("snapshot");
    assert_eq!(
        snapshot.trust.decision,
        TrustDecision::Ask,
        "malformed hook stdout must fail open by default"
    );
    application.cleanup().await;
    if let Some(runtime) = runtime {
        runtime.shutdown().await;
    }

    let fx = Fixture::new()?;
    fx.configure_hook("not json", true)?;
    let (application, runtime) = build_application(&fx, false).await?;
    application.reload().await.context("production reload path")?;
    let snapshot = application.resource_snapshot().expect("snapshot");
    assert_eq!(
        snapshot.trust.decision,
        TrustDecision::Untrusted,
        "failClosed must deny when the hook fails"
    );
    application.cleanup().await;
    if let Some(runtime) = runtime {
        runtime.shutdown().await;
    }
    Ok(())
}

#[tokio::test]
async fn startup_fires_hooks_and_records_the_composed_decision() -> Result<()> {
    let fx = Fixture::new()?;
    fx.configure_hook(r#"{"decision":"allow"}"#, true)?;
    let (application, runtime) = build_application(&fx, true).await?;

    // `Application::new_with_extensions` already ran the startup trust
    // resolution: the extension approved the undecided project, so the
    // composed decision (Trusted) must be recorded in the live snapshot and
    // the host hook must have observed the tentative ask exactly once.
    let snapshot = application.resource_snapshot().expect("snapshot");
    assert_eq!(
        snapshot.trust.decision,
        TrustDecision::Trusted,
        "startup must record the hook-composed decision"
    );
    assert_eq!(
        fx.hook_fire_count()?,
        1,
        "startup fires the host hook exactly once"
    );
    let payload = fx.last_hook_payload()?.expect("host hook must have fired");
    assert_eq!(payload["path"], fx.cwd.to_string_lossy().as_ref());
    assert_eq!(payload["decision"], "ask");
    assert_eq!(payload["isNew"], true);

    application.cleanup().await;
    if let Some(runtime) = runtime {
        runtime.shutdown().await;
    }
    Ok(())
}

#[tokio::test]
async fn startup_without_hook_surfaces_keeps_the_plain_decision() -> Result<()> {
    // No host hooks, no extension runtime: startup must not change anything
    // and must not re-stage resources.
    let fx = Fixture::new()?;
    let resources = fx.resources();
    let session = Session::new(SessionOptions {
        model: pi_ai::Model::default(),
        cwd: fx.cwd.clone(),
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
    .context("building session")?;
    session
        .attach_resources(resources.clone())
        .await
        .context("attaching resources")?;
    let generation = resources.snapshot().generation;
    let application = Application::new(session).await;
    let snapshot = application.resource_snapshot().expect("snapshot");
    assert_eq!(snapshot.trust.decision, TrustDecision::Ask);
    assert_eq!(
        snapshot.generation, generation,
        "no hook surfaces configured: the initial build must be kept as-is"
    );
    application.cleanup().await;
    Ok(())
}
