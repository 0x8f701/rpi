//! Fail-closed project trust and resource snapshot contracts.
//!
//! These integration tests exercise only public APIs and assert observable
//! trust decisions, snapshot contents, and on-disk trust store bytes.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use pi_agent::ThinkingLevel;
use pi_ai::providers::{FauxProviderOptions, FauxProviderRegistration, register_faux_provider};
use pi_ai::{Model, SimpleStreamOptions};
use pi_coding::{
    Application, ApplicationRuntimeCandidate, DefaultProjectTrust, MAX_RESOURCE_FILE_BYTES,
    ResourceDiscovery, ResourceManager, ResourceManagerOptions, Session, SessionOptions,
    SkillSource, TrustDecision, TrustStore, context_file_diagnostics, resolve_project_trust,
};
use serde_json::Value;
use tempfile::TempDir;

struct Fixture {
    _cwd: TempDir,
    _agent: TempDir,
    cwd: PathBuf,
    agent_dir: PathBuf,
}

impl Fixture {
    fn new() -> Result<Self> {
        let cwd = TempDir::new()?;
        let agent = TempDir::new()?;
        let cwd_path = cwd.path().canonicalize()?;
        let agent_path = agent.path().canonicalize()?;
        Ok(Self {
            _cwd: cwd,
            _agent: agent,
            cwd: cwd_path,
            agent_dir: agent_path,
        })
    }

    fn options(&self, headless: bool, override_trust: Option<bool>) -> ResourceManagerOptions {
        let mut options = ResourceManagerOptions::new(self.cwd.clone());
        options.agent_dir = self.agent_dir.clone();
        options.headless = headless;
        options.project_trust_override = override_trust;
        options
    }

    fn write_project_skill(&self, name: &str, description: &str) -> Result<PathBuf> {
        let dir = self.cwd.join(".rpi").join("skills").join(name);
        fs::create_dir_all(&dir)?;
        let path = dir.join("SKILL.md");
        fs::write(
            &path,
            format!("---\nname: {name}\ndescription: {description}\n---\n# {name}\n"),
        )?;
        Ok(path)
    }

    fn write_global_settings(&self, body: &str) -> Result<()> {
        fs::create_dir_all(&self.agent_dir)?;
        fs::write(self.agent_dir.join("settings.json"), body)?;
        Ok(())
    }

    fn trust_store(&self) -> TrustStore {
        TrustStore::new(&self.agent_dir)
    }

    fn trust_bytes(&self) -> Result<Option<Vec<u8>>> {
        let path = self.agent_dir.join("trust.json");
        match fs::read(&path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }
}

fn skill_names(manager: &ResourceManager) -> Vec<String> {
    let mut names = manager
        .snapshot()
        .skills
        .iter()
        .map(|skill| skill.name.clone())
        .collect::<Vec<_>>();
    names.sort();
    names
}

fn project_skill_names(manager: &ResourceManager) -> Vec<String> {
    let mut names = manager
        .snapshot()
        .skills
        .iter()
        .filter(|skill| skill.source == SkillSource::Project)
        .map(|skill| skill.name.clone())
        .collect::<Vec<_>>();
    names.sort();
    names
}

#[test]
fn ask_plus_headless_fails_closed_for_project_resources() -> Result<()> {
    let fx = Fixture::new()?;
    fx.write_project_skill(
        "secret-deploy",
        "project-only skill that must stay excluded when untrusted",
    )?;

    struct Case {
        name: &'static str,
        headless: bool,
        default_settings: Option<&'static str>,
        expect_decision: TrustDecision,
        expect_allows: bool,
    }

    let cases = [
        Case {
            name: "default ask headless",
            headless: true,
            default_settings: None,
            expect_decision: TrustDecision::Untrusted,
            expect_allows: false,
        },
        Case {
            name: "explicit ask headless",
            headless: true,
            default_settings: Some(r#"{ "defaultProjectTrust": "ask" }"#),
            expect_decision: TrustDecision::Untrusted,
            expect_allows: false,
        },
        Case {
            name: "default never headless",
            headless: true,
            default_settings: Some(r#"{ "defaultProjectTrust": "never" }"#),
            expect_decision: TrustDecision::Untrusted,
            expect_allows: false,
        },
        Case {
            name: "interactive ask stays ask and still denies resources",
            headless: false,
            default_settings: Some(r#"{ "defaultProjectTrust": "ask" }"#),
            expect_decision: TrustDecision::Ask,
            expect_allows: false,
        },
    ];

    for case in cases {
        if let Some(settings) = case.default_settings {
            fx.write_global_settings(settings)?;
        } else if fx.agent_dir.join("settings.json").exists() {
            fs::remove_file(fx.agent_dir.join("settings.json"))?;
        }

        let manager = ResourceManager::new(fx.options(case.headless, None))?;
        let snapshot = manager.snapshot();
        assert_eq!(
            snapshot.trust.decision, case.expect_decision,
            "{}: trust decision",
            case.name
        );
        assert_eq!(
            snapshot.trust.allows_project_resources(case.headless),
            case.expect_allows,
            "{}: allows_project_resources",
            case.name
        );
        assert!(
            !snapshot.trust.is_trusted(),
            "{}: is_trusted must be false",
            case.name
        );
        assert!(
            project_skill_names(&manager).is_empty(),
            "{}: project skills must be excluded, got {:?}",
            case.name,
            project_skill_names(&manager)
        );
        assert!(
            !skill_names(&manager)
                .iter()
                .any(|name| name == "secret-deploy"),
            "{}: secret project skill leaked into snapshot",
            case.name
        );
    }

    Ok(())
}

#[test]
fn one_run_override_trusts_without_persisting_trust_store() -> Result<()> {
    let fx = Fixture::new()?;
    fx.write_project_skill(
        "ephemeral-skill",
        "visible only through one-run approve override",
    )?;
    let before = fx.trust_bytes()?;

    struct Case {
        name: &'static str,
        override_trust: bool,
        expect_decision: TrustDecision,
        expect_project_skill: bool,
    }

    let cases = [
        Case {
            name: "approve override",
            override_trust: true,
            expect_decision: TrustDecision::Trusted,
            expect_project_skill: true,
        },
        Case {
            name: "no-approve override",
            override_trust: false,
            expect_decision: TrustDecision::Untrusted,
            expect_project_skill: false,
        },
    ];

    for case in cases {
        // Ensure no prior persistence confuses the non-persistence contract.
        let _ = fs::remove_file(fx.agent_dir.join("trust.json"));

        let resolution = resolve_project_trust(
            &fx.trust_store(),
            &fx.cwd,
            Some(case.override_trust),
            DefaultProjectTrust::Ask,
            true,
        )?;
        assert_eq!(
            resolution.decision, case.expect_decision,
            "{}: resolve_project_trust decision",
            case.name
        );
        assert_eq!(
            resolution.matched_path, None,
            "{}: one-run override must not claim a stored match",
            case.name
        );
        assert!(
            fx.trust_bytes()?.is_none(),
            "{}: resolve_project_trust must not create trust.json",
            case.name
        );

        let manager = ResourceManager::new(fx.options(true, Some(case.override_trust)))?;
        let snapshot = manager.snapshot();
        assert_eq!(
            snapshot.trust.decision, case.expect_decision,
            "{}: manager decision",
            case.name
        );
        assert_eq!(
            snapshot.trust.is_trusted(),
            case.override_trust,
            "{}: is_trusted",
            case.name
        );
        assert_eq!(
            project_skill_names(&manager).contains(&"ephemeral-skill".to_owned()),
            case.expect_project_skill,
            "{}: project skill inclusion {:?}",
            case.name,
            project_skill_names(&manager)
        );

        let after = fx.trust_bytes()?;
        assert_eq!(
            after, before,
            "{}: one-run override must leave trust.json bytes unchanged",
            case.name
        );
        if let Some(bytes) = after.as_ref() {
            let text = String::from_utf8_lossy(bytes);
            assert!(
                !text.contains(fx.cwd.to_string_lossy().as_ref()),
                "{}: cwd must not appear in trust store bytes",
                case.name
            );
        }
    }

    // Persisted decisions still work and remain the only durable path.
    fx.trust_store().set(&fx.cwd, TrustDecision::Trusted)?;
    let persisted = fx
        .trust_bytes()?
        .expect("trust.json must exist after TrustStore::set");
    let document: Value = serde_json::from_slice(&persisted)?;
    assert_eq!(document["version"], 1);
    let decisions = document["decisions"].as_object().expect("decisions object");
    let key = fx.cwd.to_string_lossy();
    assert_eq!(
        decisions.get(key.as_ref()).and_then(Value::as_bool),
        Some(true),
        "persisted trusted decision must be visible in file bytes"
    );

    Ok(())
}

#[test]
fn git_worktree_root_is_the_shared_trust_identity() -> Result<()> {
    let root = TempDir::new()?;
    let agent = TempDir::new()?;
    let repository = root.path().join("workspace");
    let sibling_a = repository.join("crate-a").join("nested");
    let sibling_b = repository.join("crate-b");
    fs::create_dir_all(repository.join(".git"))?;
    fs::write(repository.join(".git/HEAD"), "ref: refs/heads/main\n")?;
    fs::create_dir_all(&sibling_a)?;
    fs::create_dir_all(&sibling_b)?;
    fs::write(repository.join("AGENTS.md"), "shared repository rules")?;
    let repository = repository.canonicalize()?;

    let store = TrustStore::new(agent.path());
    store.set(&sibling_a, TrustDecision::Trusted)?;

    for cwd in [&sibling_a, &sibling_b] {
        let resolution = resolve_project_trust(&store, cwd, None, DefaultProjectTrust::Ask, true)?;
        assert_eq!(resolution.decision, TrustDecision::Trusted);
        assert_eq!(resolution.project_path, repository);
        assert_eq!(
            resolution.matched_path.as_deref(),
            Some(repository.as_path())
        );
    }

    let document: Value = serde_json::from_slice(&fs::read(agent.path().join("trust.json"))?)?;
    let decisions = document["decisions"].as_object().expect("decisions");
    assert_eq!(decisions.len(), 1);
    assert_eq!(
        decisions
            .get(repository.to_string_lossy().as_ref())
            .and_then(Value::as_bool),
        Some(true),
    );
    Ok(())
}

#[test]
fn untrusted_project_resources_excluded_trusted_resources_included() -> Result<()> {
    let fx = Fixture::new()?;
    fx.write_project_skill(
        "project-alpha",
        "alpha skill used to prove trust gating of discovery",
    )?;
    fx.write_project_skill(
        "project-beta",
        "beta skill used to prove trust gating of discovery",
    )?;
    // Project settings must also stay out until trusted.
    fs::create_dir_all(fx.cwd.join(".rpi"))?;
    fs::write(
        fx.cwd.join(".rpi").join("settings.json"),
        r#"{ "theme": "project-only-theme", "defaultModel": "project/model" }"#,
    )?;

    struct Case {
        name: &'static str,
        override_trust: Option<bool>,
        persist: Option<TrustDecision>,
        expect_trusted: bool,
        expect_skills: &'static [&'static str],
        expect_theme: Option<&'static str>,
        expect_model: Option<&'static str>,
    }

    let cases = [
        Case {
            name: "headless ask excludes project resources",
            override_trust: None,
            persist: None,
            expect_trusted: false,
            expect_skills: &[],
            expect_theme: None,
            expect_model: None,
        },
        Case {
            name: "persisted untrusted excludes project resources",
            override_trust: None,
            persist: Some(TrustDecision::Untrusted),
            expect_trusted: false,
            expect_skills: &[],
            expect_theme: None,
            expect_model: None,
        },
        Case {
            name: "one-run approve includes project resources",
            override_trust: Some(true),
            persist: None,
            expect_trusted: true,
            expect_skills: &["project-alpha", "project-beta"],
            expect_theme: Some("project-only-theme"),
            expect_model: Some("project/model"),
        },
        Case {
            name: "persisted trusted includes project resources",
            override_trust: None,
            persist: Some(TrustDecision::Trusted),
            expect_trusted: true,
            expect_skills: &["project-alpha", "project-beta"],
            expect_theme: Some("project-only-theme"),
            expect_model: Some("project/model"),
        },
        Case {
            name: "one-run deny wins over persisted trusted",
            override_trust: Some(false),
            persist: Some(TrustDecision::Trusted),
            expect_trusted: false,
            expect_skills: &[],
            expect_theme: None,
            expect_model: None,
        },
    ];

    for case in cases {
        let _ = fs::remove_file(fx.agent_dir.join("trust.json"));
        if let Some(decision) = case.persist {
            fx.trust_store().set(&fx.cwd, decision)?;
        }

        let manager = ResourceManager::new(fx.options(true, case.override_trust))?;
        let snapshot = manager.snapshot();
        assert_eq!(
            snapshot.trust.is_trusted(),
            case.expect_trusted,
            "{}: is_trusted",
            case.name
        );
        assert_eq!(
            snapshot.trust.allows_project_resources(true),
            case.expect_trusted,
            "{}: allows_project_resources",
            case.name
        );

        let mut expected = case
            .expect_skills
            .iter()
            .map(|name| (*name).to_owned())
            .collect::<Vec<_>>();
        expected.sort();
        assert_eq!(
            project_skill_names(&manager),
            expected,
            "{}: project skills",
            case.name
        );

        for skill in snapshot
            .skills
            .iter()
            .filter(|skill| skill.source == SkillSource::Project)
        {
            assert!(
                skill.trusted,
                "{}: included project skill {} must be marked trusted",
                case.name, skill.name
            );
            assert!(
                Path::new(&skill.file_path).starts_with(fx.cwd.join(".rpi").join("skills")),
                "{}: skill path must stay under project .rpi/skills: {}",
                case.name,
                skill.file_path
            );
        }

        assert_eq!(
            snapshot.settings.theme.as_deref(),
            case.expect_theme,
            "{}: project theme settings merge",
            case.name
        );
        assert_eq!(
            snapshot.settings.default_model.as_deref(),
            case.expect_model,
            "{}: project defaultModel merge",
            case.name
        );
    }

    Ok(())
}

/// Builds a deterministic, trusted-project Application over the fixture with
/// context files and project skills ENABLED (the resource proofs must
/// exercise the real discovery path). `danger-full-access` skips the launcher
/// probe, so bootstrap and every reload run identically on every host.
async fn bootstrap_trusted_application(
    fx: &Fixture,
) -> Result<(Application, FauxProviderRegistration, ResourceManager)> {
    let suffix = uuid::Uuid::now_v7().to_string();
    let api = format!("trust-resource-contract-api-{suffix}");
    let provider = format!("trust-resource-contract-provider-{suffix}");
    let model = Model {
        id: "trust-resource-contract-model".to_owned(),
        name: "Trust Resource Contract Model".to_owned(),
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
    let session = Session::new_with_additional_tools_filtered_and_discovery(
        SessionOptions {
            model,
            cwd: fx.cwd.clone(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: String::new(),
            compaction: None,
            stream_options: SimpleStreamOptions::default(),
            tools: Some(Vec::new()),
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: None,
            auth_resolver: None,
            auth_manager: None,
        },
        Vec::new(),
        pi_coding::ToolSelection::default(),
        ResourceDiscovery::Disabled,
    )?;
    let session_dir = fx.cwd.join(format!(".trust-contract-sessions-{suffix}"));
    fs::create_dir_all(&session_dir)?;
    session.set_session_dir(session_dir);
    let resources = ResourceManager::new(fx.options(true, Some(true)))?;
    session.attach_resources(resources.clone()).await?;
    let application =
        Application::from_runtime_candidate(ApplicationRuntimeCandidate::new(session))
            .await
            .expect("bootstrap trusted application");
    Ok((application, registration, resources))
}

/// A failing reload stages invisibly: the prior generation, trust decision,
/// and snapshot bytes survive a candidate that fails validation, and reload
/// only advances after the candidate is repaired. Corrupting the trusted
/// project keybindings must never publish the invalid file or bump the
/// generation; repairing it must let the next reload advance exactly once.
#[tokio::test]
async fn failed_reload_preserves_prior_generation_and_snapshot() -> Result<()> {
    let fx = Fixture::new()?;
    fx.write_project_skill(
        "stable-skill",
        "skill present in the committed generation before a bad reload",
    )?;
    fx.write_global_settings(r#"{"sandbox":{"mode":"danger-full-access"}}"#)?;
    let (application, _registration, resources) = bootstrap_trusted_application(&fx).await?;

    let before = application.resource_snapshot().expect("snapshot before");
    assert_eq!(before.generation, 1);
    assert_eq!(before.trust.decision, TrustDecision::Trusted);
    assert_eq!(
        project_skill_names(&resources),
        ["stable-skill".to_owned()],
        "precondition: the trusted project skill must load"
    );
    let before_skill_paths = before
        .skills
        .iter()
        .map(|skill| skill.file_path.clone())
        .collect::<Vec<_>>();

    // Corrupt project keybindings so candidate validation fails on reload.
    fs::write(
        fx.cwd.join(".rpi").join("keybindings.json"),
        r#""not-an-object""#,
    )?;

    let error = application
        .reload()
        .await
        .expect_err("invalid project keybindings must fail reload");
    let message = format!("{error:#}");
    assert!(
        message.contains("keybindings") || message.contains("object"),
        "reload error should name the keybindings/JSON validation, got: {message}"
    );

    let after = application
        .resource_snapshot()
        .expect("snapshot after failure");
    assert_eq!(
        after.generation, before.generation,
        "failed reload must preserve generation"
    );
    assert_eq!(resources.generation(), before.generation);
    assert_eq!(after.trust, before.trust);
    assert_eq!(
        project_skill_names(&resources),
        ["stable-skill".to_owned()],
        "failed reload must keep the committed project skills"
    );
    assert_eq!(
        after
            .skills
            .iter()
            .map(|skill| skill.file_path.clone())
            .collect::<Vec<_>>(),
        before_skill_paths
    );
    // The bad file must not have been adopted into the live snapshot paths.
    assert!(
        !after
            .keybinding_files
            .iter()
            .any(|path| path.ends_with("keybindings.json") && path.starts_with(fx.cwd.join(".rpi"))),
        "failed reload must not publish invalid project keybindings into the live snapshot"
    );

    // Repair and prove reload can advance only after validation succeeds.
    fs::write(fx.cwd.join(".rpi").join("keybindings.json"), "{}\n")?;
    let reloaded = application
        .reload()
        .await
        .expect("repaired reload must succeed");
    assert_eq!(reloaded.generation, before.generation + 1);
    assert_eq!(resources.generation(), before.generation + 1);
    assert_eq!(
        project_skill_names(&resources),
        ["stable-skill".to_owned()],
        "repaired reload must keep trusted project skills"
    );
    assert!(
        resources
            .snapshot()
            .keybinding_files
            .iter()
            .any(|path| path == &fx.cwd.join(".rpi").join("keybindings.json")),
        "successful reload may publish valid project keybindings"
    );

    Ok(())
}

/// Invalid UTF-8 and oversize context candidates fail reload atomically: the
/// prior snapshot bytes stay live, the generation never advances, and the
/// failure carries typed per-file diagnostics naming the exact candidate and
/// reason. Removing the bad candidate lets the next reload advance.
#[tokio::test]
async fn invalid_and_oversize_context_candidates_fail_atomically() -> Result<()> {
    let fx = Fixture::new()?;
    fx.write_global_settings(r#"{"sandbox":{"mode":"danger-full-access"}}"#)?;
    fs::write(fx.cwd.join("AGENTS.md"), "stable")?;
    let (application, _registration, resources) = bootstrap_trusted_application(&fx).await?;
    assert_eq!(application.resource_generation(), Some(1));
    assert_eq!(
        resources.snapshot().context_files[0].content,
        "stable",
        "precondition: the committed context file must load"
    );

    // Invalid UTF-8 candidate: reload fails with a typed context diagnostic
    // and the committed snapshot stays byte-for-byte live.
    fs::write(fx.cwd.join("CLAUDE.md"), [0xff, 0xfe])?;
    let error = application
        .reload()
        .await
        .expect_err("invalid UTF-8 candidate must fail reload");
    let diagnostics = context_file_diagnostics(&error).expect("typed context diagnostics");
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    assert_eq!(diagnostics[0].path, fx.cwd.join("CLAUDE.md"));
    assert!(
        diagnostics[0].message.contains("UTF-8"),
        "the diagnostic must name the UTF-8 failure: {diagnostics:?}"
    );
    assert_eq!(
        application.resource_generation(),
        Some(1),
        "failed reload must not advance the published generation"
    );
    assert_eq!(
        resources.snapshot().context_files[0].content,
        "stable",
        "the committed context bytes must survive the failed reload"
    );

    // Oversize candidate: same atomic failure with the byte-limit reason.
    fs::remove_file(fx.cwd.join("CLAUDE.md"))?;
    let mut file = fs::File::create(fx.cwd.join("CLAUDE.md"))?;
    file.set_len(MAX_RESOURCE_FILE_BYTES + 1)?;
    let error = application
        .reload()
        .await
        .expect_err("oversize candidate must fail reload");
    let diagnostics = context_file_diagnostics(&error).expect("typed context diagnostics");
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    assert_eq!(diagnostics[0].path, fx.cwd.join("CLAUDE.md"));
    assert!(
        diagnostics[0].message.contains("bytes"),
        "the diagnostic must name the byte limit: {diagnostics:?}"
    );
    assert_eq!(application.resource_generation(), Some(1));

    // Removing the bad candidate lets reload advance exactly once.
    fs::remove_file(fx.cwd.join("CLAUDE.md"))?;
    let reloaded = application
        .reload()
        .await
        .expect("repaired reload must succeed");
    assert_eq!(reloaded.generation, 2);
    assert_eq!(application.resource_generation(), Some(2));
    Ok(())
}

#[test]
fn no_project_config_is_implicitly_trusted_without_store_entry() -> Result<()> {
    let fx = Fixture::new()?;
    // No .rpi directory at all.
    let resolution = resolve_project_trust(
        &fx.trust_store(),
        &fx.cwd,
        None,
        DefaultProjectTrust::Ask,
        true,
    )?;
    assert_eq!(resolution.decision, TrustDecision::Trusted);
    assert_eq!(resolution.matched_path, None);
    assert!(fx.trust_bytes()?.is_none());

    let manager = ResourceManager::new(fx.options(true, None))?;
    assert!(manager.snapshot().trust.is_trusted());
    assert!(project_skill_names(&manager).is_empty());
    assert!(fx.trust_bytes()?.is_none());
    Ok(())
}

#[test]
fn agents_only_project_requires_trust_and_is_boundary_scoped() -> Result<()> {
    let root = TempDir::new()?;
    let agent = TempDir::new()?;
    let repository = root.path().join("repository");
    let cwd = repository.join("nested");
    fs::create_dir_all(repository.join(".git"))?;
    fs::write(repository.join(".git/HEAD"), "ref: refs/heads/main\n")?;
    fs::create_dir_all(&cwd)?;
    fs::write(repository.join("AGENTS.md"), "repository rules")?;
    fs::write(root.path().join("AGENTS.md"), "outside rules")?;

    let mut options = ResourceManagerOptions::new(&cwd);
    options.agent_dir = agent.path().to_path_buf();
    options.headless = true;
    let untrusted = ResourceManager::new(options.clone())?.snapshot();
    assert_eq!(untrusted.trust.decision, TrustDecision::Untrusted);
    assert!(untrusted.context_files.is_empty());
    assert_eq!(untrusted.trust.project_path, repository.canonicalize()?);

    options.project_trust_override = Some(true);
    let trusted = ResourceManager::new(options)?.snapshot();
    assert_eq!(trusted.context_files.len(), 1);
    assert_eq!(trusted.context_files[0].path, "AGENTS.md");
    assert_eq!(trusted.context_files[0].content, "repository rules");
    assert!(
        !trusted
            .context_files
            .iter()
            .any(|file| file.content.contains("outside rules"))
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn global_context_symlink_under_owner_home_is_allowed() -> Result<()> {
    use std::os::unix::fs::symlink;

    let owner = TempDir::new()?;
    let agent_dir = owner.path().join(".pi/agent");
    let cwd = TempDir::new()?;
    let target = owner.path().join("shared/AGENTS.md");
    fs::create_dir_all(target.parent().expect("target parent"))?;
    fs::create_dir_all(&agent_dir)?;
    fs::write(&target, "global rules")?;
    symlink(&target, agent_dir.join("AGENTS.md"))?;
    let mut options = ResourceManagerOptions::new(cwd.path());
    options.agent_dir = agent_dir;
    options.project_trust_override = Some(true);

    let snapshot = ResourceManager::new(options)?.snapshot();
    assert_eq!(snapshot.context_files.len(), 1);
    assert_eq!(snapshot.context_files[0].path, "<agent-dir>/AGENTS.md");
    assert_eq!(snapshot.context_files[0].content, "global rules");
    Ok(())
}

#[cfg(unix)]
#[test]
fn global_context_symlink_outside_owner_home_is_allowed() -> Result<()> {
    use std::os::unix::fs::symlink;

    let owner = TempDir::new()?;
    let outside = TempDir::new()?;
    let cwd = TempDir::new()?;
    let agent_dir = owner.path().join(".pi/agent");
    fs::create_dir_all(&agent_dir)?;
    let target = outside.path().join("AGENTS.md");
    fs::write(&target, "external global rules")?;
    symlink(&target, agent_dir.join("AGENTS.md"))?;
    let mut options = ResourceManagerOptions::new(cwd.path());
    options.agent_dir = agent_dir;
    options.project_trust_override = Some(true);

    let manager = ResourceManager::new(options)?;
    assert!(
        manager
            .snapshot()
            .context_files
            .iter()
            .any(|file| file.content.contains("external global rules"))
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn trusted_context_symlink_is_rejected_without_external_read() -> Result<()> {
    use std::os::unix::fs::symlink;

    let fx = Fixture::new()?;
    let outside = TempDir::new()?;
    fs::write(outside.path().join("rules.md"), "outside secret")?;
    symlink(outside.path().join("rules.md"), fx.cwd.join("AGENTS.md"))?;
    let error = match ResourceManager::new(fx.options(true, Some(true))) {
        Ok(_) => panic!("trusted context symlink must fail"),
        Err(error) => error,
    };
    let diagnostics = context_file_diagnostics(&error).expect("typed context diagnostics");
    assert!(
        diagnostics[0].message.contains("symbolic link"),
        "{diagnostics:?}"
    );
    assert!(!error.to_string().contains("outside secret"));
    Ok(())
}
