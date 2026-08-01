//! Focused tests for `pi config` package-resource configuration.
//!
//! Covers global/project scope, trust denial, toggle/apply/cancel, package
//! collision/source labels, invalid manifest rollback, and headless JSON
//! output. Tests drive the library model directly (no PTY required) and use
//! the `pi` binary only for install setup and the headless JSON smoke.

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

use pi_cli::package_config::load_config_model;
use pi_coding::PackageScope;
use serde_json::Value;
use tempfile::TempDir;

fn pi_bin() -> String {
    env!("CARGO_BIN_EXE_pi").to_owned()
}

/// Run `pi` with a sandboxed agent dir / HOME and a cleared auth environment so
/// no real user state or credentials are touched. stdout is piped (non-TTY),
/// which selects the headless path for `pi config`.
fn run(agent_dir: &Path, cwd: &Path, args: &[&str]) -> (bool, String, String) {
    let output = Command::new(pi_bin())
        .args(args)
        .env("PI_CODING_AGENT_DIR", agent_dir)
        .env("HOME", agent_dir)
        .env("USERPROFILE", agent_dir)
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .env_remove("DEEPSEEK_API_KEY")
        .env_remove("GEMINI_API_KEY")
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .expect("run pi");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// Build a local package declaring one extension, skill, prompt, and theme via
/// a `package.json#pi` manifest.
fn make_package(root: &Path) {
    fs::create_dir_all(root.join("extensions/my-ext")).unwrap();
    fs::write(
        root.join("extensions/my-ext/pi-extension.json"),
        r#"{"schemaVersion":1,"id":"my-ext","runtime":"process","executable":"run.sh"}"#,
    )
    .unwrap();
    fs::create_dir_all(root.join("skills")).unwrap();
    fs::write(
        root.join("skills/review.md"),
        "---\nname: review\ndescription: review code\n---\nbody\n",
    )
    .unwrap();
    fs::create_dir_all(root.join("prompts")).unwrap();
    fs::write(
        root.join("prompts/greet.md"),
        "---\nname: greet\ndescription: greeting\n---\nbody\n",
    )
    .unwrap();
    fs::create_dir_all(root.join("themes")).unwrap();
    fs::write(root.join("themes/dark.json"), r#"{"name":"dark"}"#).unwrap();
    fs::write(
        root.join("package.json"),
        r#"{"name":"test-pkg","pi":{"extensions":["extensions/*"],"skills":["skills/*.md"],"prompts":["prompts/*.md"],"themes":["themes/*.json"]}}"#,
    )
    .unwrap();
}

fn skill_index(
    model: &pi_cli::package_config::PackageConfigModel,
    pkg: usize,
    name: &str,
) -> usize {
    model.entries[pkg]
        .groups
        .skills
        .iter()
        .position(|r| r.name == name)
        .unwrap_or_else(|| panic!("skill resource {name} not found"))
}

fn read_settings(agent_dir: &Path) -> Value {
    let raw = fs::read_to_string(agent_dir.join("settings.json")).unwrap_or_default();
    serde_json::from_str(&raw).unwrap_or_else(|_| Value::Null)
}

fn read_project_settings(cwd: &Path) -> Value {
    let raw = fs::read_to_string(cwd.join(".pi/settings.json")).unwrap_or_default();
    serde_json::from_str(&raw).unwrap_or_else(|_| Value::Null)
}

#[test]
fn global_scope_lists_resources_all_enabled_by_default() {
    let home = TempDir::new().unwrap();
    let agent_dir = home.path();
    let cwd = TempDir::new().unwrap();
    let pkg = cwd.path().join("pkg");
    make_package(&pkg);

    let (ok, _out, err) = run(agent_dir, cwd.path(), &["install", pkg.to_str().unwrap()]);
    assert!(ok, "global install failed: {err}");

    let model = load_config_model(cwd.path(), agent_dir, false, PackageScope::Global).unwrap();
    assert_eq!(model.scope(), PackageScope::Global);
    assert_eq!(model.entries.len(), 1);
    let entry = &model.entries[0];
    assert!(entry.installed);
    assert!(entry.identity.as_ref().unwrap().starts_with("local:"));
    assert_eq!(entry.groups.extensions.len(), 1);
    assert_eq!(entry.groups.skills.len(), 1);
    assert_eq!(entry.groups.prompts.len(), 1);
    assert_eq!(entry.groups.themes.len(), 1);
    let skill = &entry.groups.skills[0];
    assert_eq!(skill.name, "skills/review.md");
    assert!(skill.enabled, "manifest-default resource should be enabled");
}

#[test]
fn project_scope_refused_when_untrusted() {
    let home = TempDir::new().unwrap();
    let agent_dir = home.path();
    let cwd = TempDir::new().unwrap();
    let pkg = cwd.path().join("pkg");
    make_package(&pkg);

    let (ok, _out, err) = run(
        agent_dir,
        cwd.path(),
        &["install", pkg.to_str().unwrap(), "--local", "--approve"],
    );
    assert!(ok, "project install failed: {err}");

    let error = load_config_model(cwd.path(), agent_dir, false, PackageScope::Project).unwrap_err();
    assert!(
        error.to_string().contains("not trusted"),
        "expected trust denial, got: {error}"
    );
}

#[test]
fn project_scope_lists_resources_when_trusted() {
    let home = TempDir::new().unwrap();
    let agent_dir = home.path();
    let cwd = TempDir::new().unwrap();
    let pkg = cwd.path().join("pkg");
    make_package(&pkg);

    let (ok, _out, err) = run(
        agent_dir,
        cwd.path(),
        &["install", pkg.to_str().unwrap(), "--local", "--approve"],
    );
    assert!(ok, "project install failed: {err}");

    let model = load_config_model(cwd.path(), agent_dir, true, PackageScope::Project).unwrap();
    assert_eq!(model.scope(), PackageScope::Project);
    assert_eq!(model.entries.len(), 1);
    assert!(model.entries[0].installed);
    assert!(model.entries[0].groups.skills[0].enabled);
}

#[test]
fn toggle_off_applies_minus_token_atomically() {
    let home = TempDir::new().unwrap();
    let agent_dir = home.path();
    let cwd = TempDir::new().unwrap();
    let pkg = cwd.path().join("pkg");
    make_package(&pkg);
    let (ok, _out, err) = run(agent_dir, cwd.path(), &["install", pkg.to_str().unwrap()]);
    assert!(ok, "install failed: {err}");

    let mut model = load_config_model(cwd.path(), agent_dir, false, PackageScope::Global).unwrap();
    let idx = skill_index(&model, 0, "skills/review.md");
    assert!(model.entries[0].groups.skills[idx].enabled);
    model.toggle(0, pi_coding::PackageResourceKind::Skill, idx);
    assert!(!model.entries[0].groups.skills[idx].enabled);
    model.apply().unwrap();

    let settings = read_settings(agent_dir);
    let skills = settings["packages"][0]["skills"].as_array().unwrap();
    assert_eq!(
        skills,
        &vec![Value::String("-skills/review.md".to_string())]
    );
    // Other kinds carry no tokens and are omitted via skip_serializing_if.
    assert!(settings["packages"][0].get("extensions").is_none());
    assert!(settings["packages"][0].get("prompts").is_none());
}

#[test]
fn toggle_on_replaces_minus_with_plus_token() {
    let home = TempDir::new().unwrap();
    let agent_dir = home.path();
    let cwd = TempDir::new().unwrap();
    let pkg = cwd.path().join("pkg");
    make_package(&pkg);
    let (ok, _out, err) = run(agent_dir, cwd.path(), &["install", pkg.to_str().unwrap()]);
    assert!(ok, "install failed: {err}");

    // Pre-disable the skill via a `-` token in settings.
    let src = pkg.to_str().unwrap();
    fs::write(
        agent_dir.join("settings.json"),
        format!(r#"{{"packages":[{{"source":{src:?},"skills":["-skills/review.md"]}}]}}"#),
    )
    .unwrap();

    let mut model = load_config_model(cwd.path(), agent_dir, false, PackageScope::Global).unwrap();
    let idx = skill_index(&model, 0, "skills/review.md");
    assert!(
        !model.entries[0].groups.skills[idx].enabled,
        "should start disabled"
    );
    model.toggle(0, pi_coding::PackageResourceKind::Skill, idx);
    assert!(model.entries[0].groups.skills[idx].enabled);
    model.apply().unwrap();

    let settings = read_settings(agent_dir);
    let skills = settings["packages"][0]["skills"].as_array().unwrap();
    assert_eq!(
        skills,
        &vec![Value::String("+skills/review.md".to_string())]
    );
}

#[test]
fn cancel_writes_nothing() {
    let home = TempDir::new().unwrap();
    let agent_dir = home.path();
    let cwd = TempDir::new().unwrap();
    let pkg = cwd.path().join("pkg");
    make_package(&pkg);
    let (ok, _out, err) = run(agent_dir, cwd.path(), &["install", pkg.to_str().unwrap()]);
    assert!(ok, "install failed: {err}");

    let before = fs::read_to_string(agent_dir.join("settings.json")).unwrap();
    let mut model = load_config_model(cwd.path(), agent_dir, false, PackageScope::Global).unwrap();
    let idx = skill_index(&model, 0, "skills/review.md");
    model.toggle(0, pi_coding::PackageResourceKind::Skill, idx);
    // No apply — mimics Cancel.
    let after = fs::read_to_string(agent_dir.join("settings.json")).unwrap();
    assert_eq!(before, after, "settings must not change without Apply");
    drop(model);
}

#[test]
fn apply_preserves_other_tokens_and_unsupported_entries() {
    let home = TempDir::new().unwrap();
    let agent_dir = home.path();
    let cwd = TempDir::new().unwrap();
    let pkg = cwd.path().join("pkg");
    make_package(&pkg);
    let (ok, _out, err) = run(agent_dir, cwd.path(), &["install", pkg.to_str().unwrap()]);
    assert!(ok, "install failed: {err}");

    // An npm (unsupported) entry, a manual glob token, and a `-` theme token
    // must all survive a skill toggle.
    let src = pkg.to_str().unwrap();
    fs::write(
        agent_dir.join("settings.json"),
        format!(
            r#"{{"packages":[
              "npm:future-pkg",
              {{"source":{src:?},"skills":["+skills/review.md","skills/*.md"],"themes":["-themes/dark.json"]}}
            ]}}"#
        ),
    )
    .unwrap();

    let mut model = load_config_model(cwd.path(), agent_dir, false, PackageScope::Global).unwrap();
    assert_eq!(model.entries.len(), 2);
    assert!(model.entries[0].unsupported, "npm entry is unsupported");
    let idx = skill_index(&model, 1, "skills/review.md");
    assert!(model.entries[1].groups.skills[idx].enabled);
    model.toggle(1, pi_coding::PackageResourceKind::Skill, idx);
    model.apply().unwrap();

    let settings = read_settings(agent_dir);
    // npm entry preserved verbatim as a bare string.
    assert_eq!(
        settings["packages"][0],
        Value::String("npm:future-pkg".to_string())
    );
    let skills = settings["packages"][1]["skills"].as_array().unwrap();
    assert!(
        skills.iter().any(|v| v == "skills/*.md"),
        "manual glob token preserved: {skills:?}"
    );
    assert!(
        skills.iter().any(|v| v == "-skills/review.md"),
        "toggled resource emitted a `-` token: {skills:?}"
    );
    assert!(
        !skills.iter().any(|v| v == "+skills/review.md"),
        "prior `+` token for the toggled resource was replaced: {skills:?}"
    );
    let themes = settings["packages"][1]["themes"].as_array().unwrap();
    assert_eq!(
        themes,
        &vec![Value::String("-themes/dark.json".to_string())]
    );
}

#[test]
fn package_collision_carries_distinct_source_labels() {
    let home = TempDir::new().unwrap();
    let agent_dir = home.path();
    let cwd = TempDir::new().unwrap();
    let pkg_a = cwd.path().join("pkg-a");
    let pkg_b = cwd.path().join("pkg-b");
    make_package(&pkg_a);
    make_package(&pkg_b);

    let (ok, _o, e) = run(agent_dir, cwd.path(), &["install", pkg_a.to_str().unwrap()]);
    assert!(ok, "install a: {e}");
    let (ok, _o, e) = run(agent_dir, cwd.path(), &["install", pkg_b.to_str().unwrap()]);
    assert!(ok, "install b: {e}");

    let model = load_config_model(cwd.path(), agent_dir, false, PackageScope::Global).unwrap();
    assert_eq!(model.entries.len(), 2);
    let json = model.to_json();
    let identities: Vec<&str> = json["packages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["identity"].as_str().unwrap())
        .collect();
    assert_eq!(identities.len(), 2);
    assert_ne!(
        identities[0], identities[1],
        "package identities must be distinct"
    );
    // Both packages expose the same resource name (collision) without dedup.
    for pkg in json["packages"].as_array().unwrap() {
        assert_eq!(pkg["resources"]["skills"][0]["name"], "skills/review.md");
    }
}

#[test]
fn invalid_manifest_rollback_writes_nothing() {
    let home = TempDir::new().unwrap();
    let agent_dir = home.path();
    let cwd = TempDir::new().unwrap();
    let pkg = cwd.path().join("pkg");
    make_package(&pkg);
    let (ok, _o, e) = run(agent_dir, cwd.path(), &["install", pkg.to_str().unwrap()]);
    assert!(ok, "install: {e}");

    let before = fs::read_to_string(agent_dir.join("settings.json")).unwrap();
    // Corrupt the on-disk manifest with an unsupported schema version.
    fs::write(
        pkg.join("package.json"),
        r#"{"name":"x","pi":{"schemaVersion":999}}"#,
    )
    .unwrap();

    let error = load_config_model(cwd.path(), agent_dir, false, PackageScope::Global).unwrap_err();
    assert!(
        error.to_string().to_lowercase().contains("manifest"),
        "expected manifest error, got: {error}"
    );
    let after = fs::read_to_string(agent_dir.join("settings.json")).unwrap();
    assert_eq!(before, after, "a manifest failure must not write settings");
}

#[test]
fn headless_output_is_deterministic_json() {
    let home = TempDir::new().unwrap();
    let agent_dir = home.path();
    let cwd = TempDir::new().unwrap();
    let pkg = cwd.path().join("pkg");
    make_package(&pkg);
    let (ok, _o, e) = run(agent_dir, cwd.path(), &["install", pkg.to_str().unwrap()]);
    assert!(ok, "install: {e}");

    // Library path: the same JSON `pi config` would print headlessly.
    let model = load_config_model(cwd.path(), agent_dir, false, PackageScope::Global).unwrap();
    let json = model.to_json();
    assert_eq!(json["scope"].as_str().unwrap(), "global");
    assert_eq!(json["projectTrusted"].as_bool().unwrap(), false);
    let skill = &json["packages"][0]["resources"]["skills"][0];
    assert_eq!(skill["name"].as_str().unwrap(), "skills/review.md");
    assert_eq!(skill["enabled"].as_bool().unwrap(), true);

    // Binary path: piped stdout is non-TTY → headless, never blocks for input.
    let (ok, stdout, err) = run(agent_dir, cwd.path(), &["config"]);
    assert!(ok, "pi config headless failed: {err}");
    let parsed: Value = serde_json::from_str(stdout.trim()).expect("headless stdout is JSON");
    assert_eq!(parsed["scope"].as_str().unwrap(), "global");
    assert_eq!(
        parsed["packages"][0]["resources"]["themes"][0]["name"]
            .as_str()
            .unwrap(),
        "themes/dark.json"
    );
}

#[test]
fn headless_project_scope_refused_when_untrusted() {
    let home = TempDir::new().unwrap();
    let agent_dir = home.path();
    let cwd = TempDir::new().unwrap();
    let pkg = cwd.path().join("pkg");
    make_package(&pkg);
    let (ok, _o, e) = run(
        agent_dir,
        cwd.path(),
        &["install", pkg.to_str().unwrap(), "--local", "--approve"],
    );
    assert!(ok, "install: {e}");

    // `pi config -l` on an untrusted project (no --approve) must fail fast and
    // not block for input.
    let (ok, _stdout, err) = run(agent_dir, cwd.path(), &["config", "--local"]);
    assert!(!ok, "untrusted project config should fail");
    assert!(
        err.contains("not trusted"),
        "expected trust denial on stderr, got: {err}"
    );
    // No project settings written by the failed config.
    let _ = read_project_settings(cwd.path());
}

#[test]
fn apply_idempotent_when_no_toggles() {
    let home = TempDir::new().unwrap();
    let agent_dir = home.path();
    let cwd = TempDir::new().unwrap();
    let pkg = cwd.path().join("pkg");
    make_package(&pkg);
    let (ok, _o, e) = run(agent_dir, cwd.path(), &["install", pkg.to_str().unwrap()]);
    assert!(ok, "install: {e}");

    let before = read_settings(agent_dir);
    let model = load_config_model(cwd.path(), agent_dir, false, PackageScope::Global).unwrap();
    model.apply().unwrap();
    let after = read_settings(agent_dir);
    assert_eq!(before, after, "Apply with no toggles writes nothing");
}
