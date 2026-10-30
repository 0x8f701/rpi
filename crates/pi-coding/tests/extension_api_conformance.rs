//! Extension API conformance driven through Application-owned staging.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::Result;
use pi_ai::Model;
use pi_ai::providers::{FauxProviderOptions, FauxProviderRegistration, register_faux_provider};
use pi_coding::{
    Application, ApplicationRuntimeCandidate, ExtensionCapability, ExtensionMode,
    ExtensionPermissionSet, ExtensionRuntime, ExtensionRuntimeOptions, ResourceManager,
    ResourceManagerOptions, Session, SessionOptions,
};
use serde_json::json;

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

fn permissions(
    capabilities: impl IntoIterator<Item = ExtensionCapability>,
) -> ExtensionPermissionSet {
    ExtensionPermissionSet {
        capabilities: capabilities.into_iter().collect(),
        ui_capabilities: BTreeSet::new(),
    }
}

fn write_manifest(
    project: &Path,
    id: &str,
    entry: &Path,
    permissions: &ExtensionPermissionSet,
) -> Result<PathBuf> {
    let package = project.join(id);
    std::fs::create_dir_all(&package)?;
    let entry_name = entry
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .ok_or_else(|| anyhow::anyhow!("extension entry has no UTF-8 file name"))?
        .to_owned();
    std::fs::copy(entry, package.join(&entry_name))?;
    let manifest = package.join(pi_coding::PROCESS_EXTENSION_MANIFEST_FILE);
    std::fs::write(
        &manifest,
        serde_json::to_vec_pretty(&json!({
            "schemaVersion": 1,
            "id": id,
            "runtime": "quickjs",
            "entry": entry_name,
            "capabilities": permissions.capabilities,
            "uiCapabilities": permissions.ui_capabilities,
        }))?,
    )?;
    Ok(manifest)
}

/// A session model with a resolvable provider. The default `Model::default()`
/// has an empty `api`, which makes `SessionHttpRequirements::derive_with_builtins`
/// fail (`No API provider registered for api: ""`) when the sandbox topology
/// stages the session HTTP authorities. Registering a hermetic faux provider
/// (kept alive by the returned registration until the test drops it) gives the
/// deterministic fixture a resolvable model, exactly like the canonical
/// `extensions::bootstrap_extension_application` pattern.
fn faux_session_model() -> (Model, FauxProviderRegistration) {
    let suffix = uuid::Uuid::now_v7();
    let model = Model {
        id: format!("conformance-fixture-model-{suffix}"),
        name: "Conformance Fixture Model".to_owned(),
        api: format!("conformance-fixture-api-{suffix}"),
        provider: format!("conformance-fixture-provider-{suffix}"),
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

fn session(project: &Path, model: &Model) -> Result<Session> {
    let session_dir = project.join("sessions");
    std::fs::create_dir_all(&session_dir)?;
    let session = Session::new_with_additional_tools_filtered_and_discovery(
        SessionOptions {
            model: model.clone(),
            cwd: project.to_path_buf(),
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
    Ok(session)
}

async fn stage(
    id: &str,
    entry: &Path,
    permissions: ExtensionPermissionSet,
) -> Result<(
    Application,
    ExtensionRuntime,
    tempfile::TempDir,
    FauxProviderRegistration,
)> {
    let root = tempfile::tempdir()?;
    let project = root.path().join("project");
    std::fs::create_dir_all(&project)?;
    let manifest = write_manifest(&project, id, entry, &permissions)?;
    let (model, faux) = faux_session_model();
    let session = session(&project, &model)?;
    let mut resource_options = ResourceManagerOptions::new(&project);
    // Agent store is a sibling of the project. A store inside the project
    // boundary is rejected.
    resource_options.agent_dir = root.path().join("agent");
    resource_options.project_trust_override = Some(true);
    resource_options.disable_skills = true;
    resource_options.disable_prompt_templates = true;
    resource_options.disable_themes = true;
    resource_options.disable_context_files = true;
    resource_options.explicit_extension_paths = vec![manifest];
    std::fs::create_dir_all(&resource_options.agent_dir)?;
    // Pin the sandbox to danger-full-access so Application bootstrap does not
    // authenticate the confined bubblewrap launcher nor require a host HTTP
    // destination policy. These conformance tests exercise extension
    // registration only; the confined workspace-write path would make the
    // deterministic fixture host-dependent.
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
        ApplicationRuntimeCandidate::new(session).with_extensions(runtime.clone(), permissions),
    )
    .await?;
    Ok((application, runtime, root, faux))
}

async fn stage_error(
    id: &str,
    entry: &Path,
    permissions: ExtensionPermissionSet,
) -> Result<(anyhow::Error, ExtensionRuntime, FauxProviderRegistration)> {
    let root = tempfile::tempdir()?;
    let project = root.path().join("project");
    std::fs::create_dir_all(&project)?;
    let manifest = write_manifest(&project, id, entry, &permissions)?;
    let (model, faux) = faux_session_model();
    let session = session(&project, &model)?;
    let mut resource_options = ResourceManagerOptions::new(&project);
    resource_options.agent_dir = root.path().join("agent");
    resource_options.project_trust_override = Some(true);
    resource_options.disable_skills = true;
    resource_options.disable_prompt_templates = true;
    resource_options.disable_themes = true;
    resource_options.disable_context_files = true;
    resource_options.explicit_extension_paths = vec![manifest];
    std::fs::create_dir_all(&resource_options.agent_dir)?;
    // Pin the sandbox to danger-full-access (see `stage` above) so these
    // fixture-only conformance assertions reach the extension registration
    // error rather than failing during sandbox bootstrap.
    std::fs::write(
        resource_options.agent_dir.join("settings.json"),
        r#"{"sandbox":{"mode":"danger-full-access"}}"#,
    )?;
    std::fs::create_dir_all(project.join("sessions"))?;
    session
        .attach_resources(ResourceManager::new(resource_options)?)
        .await?;
    let runtime = ExtensionRuntime::process(None, options());
    let error = match Application::from_runtime_candidate(
        ApplicationRuntimeCandidate::new(session).with_extensions(runtime.clone(), permissions),
    )
    .await
    {
        Ok(application) => {
            application.cleanup().await;
            anyhow::bail!("extension unexpectedly passed Application bootstrap")
        }
        Err(error) => error,
    };
    Ok((error, runtime, faux))
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
        let entry = PathBuf::from(FIXTURES).join(fixture);
        let (error, runtime, _faux) = stage_error(id, &entry, permissions(capabilities)).await?;
        let message = format!("{error:#}").to_ascii_lowercase();
        assert!(
            message.contains(&expected.to_ascii_lowercase()),
            "unexpected {id} failure: {message}"
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
    let entry = PathBuf::from(FIXTURES).join("provider-registration.mjs");
    let (application, runtime, _project, _faux) = stage(
        "provider-registration",
        &entry,
        permissions([ExtensionCapability::Provider]),
    )
    .await?;
    let providers = runtime.providers();
    assert_eq!(
        providers.len(),
        1,
        "the fixture registers exactly one provider"
    );
    assert_eq!(providers[0].id, "provider-registration");
    assert_eq!(providers[0].api, "provider-registration-api");
    assert_eq!(providers[0].label.as_deref(), Some("Provider Registration"));
    assert_eq!(providers[0].capabilities, vec!["streaming".to_owned()]);
    application.cleanup().await;
    runtime.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn provider_registration_requires_the_provider_capability() -> Result<()> {
    let entry = PathBuf::from(FIXTURES).join("provider-registration.mjs");
    let (error, runtime, _faux) = stage_error(
        "provider-registration-ungranted",
        &entry,
        permissions([ExtensionCapability::Commands]),
    )
    .await?;
    assert!(
        format!("{error:#}")
            .to_ascii_lowercase()
            .contains("provider")
    );
    assert!(runtime.providers().is_empty());
    runtime.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn shipped_quickjs_example_loads_and_its_surface_is_live() -> Result<()> {
    let entry =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/quickjs_extension.mjs");
    assert!(
        entry.exists(),
        "example file must exist: {}",
        entry.display()
    );
    let granted = ExtensionPermissionSet {
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
    };
    let (application, runtime, _project, _faux) = stage("quickjs-example", &entry, granted).await?;

    let command_names = runtime
        .commands()
        .into_iter()
        .map(|command| command.name)
        .collect::<BTreeSet<_>>();
    assert!(
        command_names.contains("quickjs-hello") && command_names.contains("quickjs-session"),
        "example commands must be registered: {command_names:?}"
    );
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
    assert_eq!(
        text, "echo:works",
        "example tool must round-trip through QuickJS"
    );

    application.cleanup().await;
    runtime.shutdown().await;
    Ok(())
}
