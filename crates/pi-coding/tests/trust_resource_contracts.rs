//! Fail-closed project trust and resource snapshot contracts.
//!
//! These integration tests exercise only public APIs and assert observable
//! trust decisions, snapshot contents, and on-disk trust store bytes.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use pi_coding::{
    DefaultProjectTrust, ResourceManager, ResourceManagerOptions, SkillSource, TrustDecision,
    TrustStore, resolve_project_trust,
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
        let dir = self.cwd.join(".pi").join("skills").join(name);
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
            !skill_names(&manager).iter().any(|name| name == "secret-deploy"),
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
    let decisions = document["decisions"]
        .as_object()
        .expect("decisions object");
    let key = fx.cwd.to_string_lossy();
    assert_eq!(
        decisions.get(key.as_ref()).and_then(Value::as_bool),
        Some(true),
        "persisted trusted decision must be visible in file bytes"
    );

    Ok(())
}

#[test]
fn nearest_ancestor_trust_match_wins_over_deeper_ask() -> Result<()> {
    let root = TempDir::new()?;
    let agent = TempDir::new()?;
    let parent = root.path().join("workspace");
    let child = parent.join("crate-a");
    let grandchild = child.join("nested");
    // Every resolve target needs a non-empty `.pi` so trust is not short-circuited
    // to the "no project config => implicitly trusted" branch.
    fs::create_dir_all(parent.join(".pi"))?;
    fs::write(parent.join(".pi").join(".keep"), "")?;
    fs::create_dir_all(child.join(".pi"))?;
    fs::write(child.join(".pi").join(".keep"), "")?;
    fs::create_dir_all(grandchild.join(".pi").join("skills").join("nested-skill"))?;
    fs::write(
        grandchild
            .join(".pi")
            .join("skills")
            .join("nested-skill")
            .join("SKILL.md"),
        "---\nname: nested-skill\ndescription: nested project skill body for trust ancestry\n---\n# nested\n",
    )?;
    fs::create_dir_all(agent.path())?;

    let store = TrustStore::new(agent.path());
    let parent_canon = parent.canonicalize()?;
    let child_canon = child.canonicalize()?;
    let grandchild_canon = grandchild.canonicalize()?;

    struct Case {
        name: &'static str,
        /// Ordered store writes; later entries overwrite earlier ones for the same path.
        store_writes: Vec<(PathBuf, TrustDecision)>,
        resolve_cwd: PathBuf,
        expect_decision: TrustDecision,
        expect_matched: Option<PathBuf>,
        expect_project_skill: bool,
    }

    let cases = [
        Case {
            name: "trusted ancestor covers nested cwd",
            store_writes: vec![(parent_canon.clone(), TrustDecision::Trusted)],
            resolve_cwd: grandchild_canon.clone(),
            expect_decision: TrustDecision::Trusted,
            expect_matched: Some(parent_canon.clone()),
            expect_project_skill: true,
        },
        Case {
            name: "untrusted ancestor blocks nested cwd",
            store_writes: vec![(parent_canon.clone(), TrustDecision::Untrusted)],
            resolve_cwd: grandchild_canon.clone(),
            expect_decision: TrustDecision::Untrusted,
            expect_matched: Some(parent_canon.clone()),
            expect_project_skill: false,
        },
        Case {
            name: "nearest child decision wins over untrusted ancestor",
            store_writes: vec![
                (parent_canon.clone(), TrustDecision::Untrusted),
                (child_canon.clone(), TrustDecision::Trusted),
            ],
            resolve_cwd: grandchild_canon.clone(),
            expect_decision: TrustDecision::Trusted,
            expect_matched: Some(child_canon.clone()),
            expect_project_skill: true,
        },
        Case {
            name: "nearest child untrusted wins over trusted ancestor",
            store_writes: vec![
                (parent_canon.clone(), TrustDecision::Trusted),
                (child_canon.clone(), TrustDecision::Untrusted),
            ],
            resolve_cwd: grandchild_canon.clone(),
            expect_decision: TrustDecision::Untrusted,
            expect_matched: Some(child_canon.clone()),
            expect_project_skill: false,
        },
    ];

    for case in cases {
        let _ = fs::remove_file(agent.path().join("trust.json"));
        for (path, decision) in &case.store_writes {
            store.set(path, *decision)?;
        }

        let stored = store.resolve(&case.resolve_cwd)?;
        assert_eq!(
            stored.decision, case.expect_decision,
            "{}: TrustStore::resolve decision",
            case.name
        );
        assert_eq!(
            stored.matched_path, case.expect_matched,
            "{}: TrustStore::resolve matched_path",
            case.name
        );

        let effective = resolve_project_trust(
            &store,
            &case.resolve_cwd,
            None,
            DefaultProjectTrust::Ask,
            true,
        )?;
        assert_eq!(
            effective.decision, case.expect_decision,
            "{}: resolve_project_trust decision",
            case.name
        );
        assert_eq!(
            effective.matched_path, case.expect_matched,
            "{}: resolve_project_trust matched_path",
            case.name
        );

        let mut options = ResourceManagerOptions::new(case.resolve_cwd.clone());
        options.agent_dir = agent.path().to_path_buf();
        options.headless = true;
        options.project_trust_override = None;
        let manager = ResourceManager::new(options)?;
        let snapshot = manager.snapshot();
        assert_eq!(
            snapshot.trust.decision, case.expect_decision,
            "{}: manager decision",
            case.name
        );
        assert_eq!(
            snapshot.trust.matched_path, case.expect_matched,
            "{}: manager matched_path",
            case.name
        );
        assert_eq!(
            project_skill_names(&manager).contains(&"nested-skill".to_owned()),
            case.expect_project_skill,
            "{}: nested skill inclusion {:?}",
            case.name,
            project_skill_names(&manager)
        );

        let bytes = fs::read(agent.path().join("trust.json"))?;
        let document: Value = serde_json::from_slice(&bytes)?;
        let decisions = document["decisions"].as_object().expect("decisions");
        let matched_key = case
            .expect_matched
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned())
            .expect("matched path");
        assert!(
            decisions.contains_key(&matched_key),
            "{}: trust.json must contain matched ancestor key {matched_key}",
            case.name
        );
    }

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
    fs::create_dir_all(fx.cwd.join(".pi"))?;
    fs::write(
        fx.cwd.join(".pi").join("settings.json"),
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
                Path::new(&skill.file_path).starts_with(fx.cwd.join(".pi").join("skills")),
                "{}: skill path must stay under project .pi/skills: {}",
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

#[test]
fn failed_reload_preserves_prior_generation_and_snapshot() -> Result<()> {
    let fx = Fixture::new()?;
    fx.write_project_skill(
        "stable-skill",
        "skill present in the committed generation before a bad reload",
    )?;

    let manager = ResourceManager::new(fx.options(true, Some(true)))?;
    let before = manager.snapshot();
    assert_eq!(before.generation, 1);
    assert_eq!(before.trust.decision, TrustDecision::Trusted);
    assert!(
        project_skill_names(&manager).contains(&"stable-skill".to_owned()),
        "precondition: trusted skill must load"
    );
    let before_skill_paths = before
        .skills
        .iter()
        .map(|skill| skill.file_path.clone())
        .collect::<Vec<_>>();

    // Corrupt project keybindings so candidate validation fails on reload.
    fs::write(
        fx.cwd.join(".pi").join("keybindings.json"),
        r#""not-an-object""#,
    )?;

    let err = manager
        .reload()
        .expect_err("invalid project keybindings must fail reload");
    let message = err.to_string();
    assert!(
        message.contains("keybindings") || message.contains("JSON") || message.contains("object"),
        "reload error should mention keybindings/JSON validation, got: {message}"
    );

    let after = manager.snapshot();
    assert_eq!(
        after.generation, before.generation,
        "failed reload must preserve generation"
    );
    assert_eq!(manager.generation(), before.generation);
    assert_eq!(after.trust, before.trust);
    assert_eq!(project_skill_names(&manager), ["stable-skill".to_owned()]);
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
            .any(|path| path.ends_with("keybindings.json")
                && path.starts_with(fx.cwd.join(".pi"))),
        "failed reload must not publish invalid project keybindings into the live snapshot"
    );

    // stage_reload failure path: leave the bad file, stage should err and drop candidate.
    let stage_err = match manager.stage_reload() {
        Ok(_) => panic!("stage_reload must fail closed on invalid candidate"),
        Err(error) => error,
    };
    assert!(
        stage_err.to_string().contains("keybindings")
            || stage_err.to_string().contains("JSON")
            || stage_err.to_string().contains("object"),
        "stage error: {stage_err}"
    );
    assert_eq!(manager.generation(), before.generation);
    assert_eq!(project_skill_names(&manager), ["stable-skill".to_owned()]);

    // Repair and prove reload can advance only after validation succeeds.
    fs::write(fx.cwd.join(".pi").join("keybindings.json"), "{}\n")?;
    let reloaded = manager.reload()?;
    assert_eq!(reloaded.generation, before.generation + 1);
    assert_eq!(manager.generation(), before.generation + 1);
    assert!(
        project_skill_names(&manager).contains(&"stable-skill".to_owned()),
        "repaired reload must keep trusted project skills"
    );
    assert!(
        manager
            .snapshot()
            .keybinding_files
            .iter()
            .any(|path| path == &fx.cwd.join(".pi").join("keybindings.json")),
        "successful reload may publish valid project keybindings"
    );

    Ok(())
}

#[test]
fn no_project_config_is_implicitly_trusted_without_store_entry() -> Result<()> {
    let fx = Fixture::new()?;
    // No .pi directory at all.
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
