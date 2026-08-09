//! Binary-level coverage for the `rpi config get|set|reset|list` settings-key
//! surface (`crates/pi-cli/src/settings_config.rs`). The pure logic is
//! unit-tested; this file proves the CLI adapter contract through the REAL
//! `rpi` binary: `list` groups every catalog key, `get` prints effective
//! value + source + behavior, `set` persists through the atomic draft+apply
//! pipeline (same validation as the TUI), `reset` clears the scoped layer,
//! secrets and unknown keys are rejected, untrusted project scope refuses,
//! `--json` emits deterministic JSON, and bare `rpi config` keeps the
//! package-resource selector. No network and no credentials.

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::Value;
use tempfile::TempDir;

fn rpi_bin() -> String {
    env!("CARGO_BIN_EXE_rpi").to_owned()
}

fn run(agent_dir: &Path, cwd: &Path, args: &[&str]) -> (bool, String, String) {
    let output = Command::new(rpi_bin())
        .args(args)
        .env("PI_CODING_AGENT_DIR", agent_dir)
        .env("HOME", agent_dir)
        .env("USERPROFILE", agent_dir)
        .env("PI_SKIP_VERSION_CHECK", "1")
        .env_remove("PI_OFFLINE")
        .env_remove("PI_PROFILE")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .env_remove("DEEPSEEK_API_KEY")
        .env_remove("GEMINI_API_KEY")
        .env_remove("AWS_ACCESS_KEY_ID")
        .env_remove("AWS_SECRET_ACCESS_KEY")
        .env_remove("GOOGLE_APPLICATION_CREDENTIALS")
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .expect("run rpi config command");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn settings_file(agent_dir: &Path) -> Value {
    let raw = fs::read_to_string(agent_dir.join("settings.json")).expect("settings.json");
    serde_json::from_str(&raw).expect("settings.json parses")
}

#[test]
fn config_list_groups_every_key_and_filters_by_category() {
    let agent = TempDir::new().expect("agent dir");
    let cwd = TempDir::new().expect("cwd");

    // Full listing groups by category and shows every catalog key with its
    // provenance chip and behavior.
    let (ok, stdout, stderr) = run(agent.path(), cwd.path(), &["config", "list"]);
    assert!(ok, "config list failed: {stderr}");
    assert!(stdout.contains("Models"), "Models group: {stdout}");
    assert!(stdout.contains("TrustSecurity"), "TrustSecurity group: {stdout}");
    assert!(
        stdout.contains("retry.maxRetries") && stdout.contains("[default]"),
        "key + provenance chip: {stdout}"
    );
    assert!(
        stdout.contains("defaultThinkingLevel") && stdout.contains("(restart)"),
        "restart behavior surfaced: {stdout}"
    );

    // Category filter accepts the Debug name and the TUI tab alias.
    for category in ["RetryTransport", "Retry"] {
        let (ok, stdout, stderr) = run(
            agent.path(),
            cwd.path(),
            &["config", "list", "--category", category],
        );
        assert!(ok, "config list {category} failed: {stderr}");
        assert!(stdout.contains("retry.enabled"), "{category} has retry keys: {stdout}");
        assert!(
            !stdout.contains("compaction.enabled"),
            "{category} must exclude other categories: {stdout}"
        );
    }
    let (ok, _stdout, stderr) = run(
        agent.path(),
        cwd.path(),
        &["config", "list", "--category", "bogus"],
    );
    assert!(!ok, "unknown category must fail");
    assert!(stderr.contains("no settings match"), "actionable error: {stderr}");
}

#[test]
fn config_list_json_serializes_value_views() {
    let agent = TempDir::new().expect("agent dir");
    let cwd = TempDir::new().expect("cwd");
    let (ok, stdout, stderr) = run(agent.path(), cwd.path(), &["config", "list", "--json"]);
    assert!(ok, "config list --json failed: {stderr}");
    let views: Vec<Value> = serde_json::from_str(stdout.trim()).expect("stdout is a JSON array");
    assert!(views.len() >= 90, "full catalog listed: {}", views.len());
    let retry = views
        .iter()
        .find(|view| view["definition"]["key"] == "retry.maxRetries")
        .expect("retry.maxRetries view");
    assert_eq!(retry["effectiveValue"], 3);
    assert_eq!(retry["source"], "default");
}

#[test]
fn config_get_reports_effective_source_and_behavior() {
    let agent = TempDir::new().expect("agent dir");
    let cwd = TempDir::new().expect("cwd");
    let (ok, stdout, stderr) = run(
        agent.path(),
        cwd.path(),
        &["config", "get", "retry.maxRetries"],
    );
    assert!(ok, "config get failed: {stderr}");
    assert!(
        stdout.contains("retry.maxRetries = 3  [default]  (live)"),
        "effective value + source + behavior: {stdout}"
    );

    // get --json emits the SettingValueView.
    let (ok, stdout, stderr) = run(
        agent.path(),
        cwd.path(),
        &["config", "get", "retry.maxRetries", "--json"],
    );
    assert!(ok, "config get --json failed: {stderr}");
    let view: Value = serde_json::from_str(stdout.trim()).expect("stdout is JSON");
    assert_eq!(view["effectiveValue"], 3);
    assert_eq!(view["source"], "default");

    // Unknown and secret keys are rejected with actionable messages.
    let (ok, _stdout, stderr) = run(agent.path(), cwd.path(), &["config", "get", "no.such.key"]);
    assert!(!ok, "unknown key must fail");
    assert!(stderr.contains("unknown setting key"), "{stderr}");
    for secret in ["apiKey", "images.genApiKey", "live.sttApiKey"] {
        let (ok, _stdout, stderr) = run(agent.path(), cwd.path(), &["config", "get", secret]);
        assert!(!ok, "{secret} must be rejected");
        assert!(stderr.contains("secret material"), "{secret}: {stderr}");
    }
}

#[test]
fn config_set_reset_round_trip_persists_and_validates() {
    let agent = TempDir::new().expect("agent dir");
    let cwd = TempDir::new().expect("cwd");

    // Set persists through the draft+apply pipeline and shows the new source.
    let (ok, stdout, stderr) = run(
        agent.path(),
        cwd.path(),
        &["config", "set", "retry.maxRetries", "7"],
    );
    assert!(ok, "config set failed: {stderr}");
    assert!(
        stdout.contains("retry.maxRetries = 7  [global]"),
        "set reports the new value and source: {stdout}"
    );
    assert_eq!(settings_file(agent.path())["retry"]["maxRetries"], 7);

    // get reflects the persisted layer.
    let (ok, stdout, stderr) = run(
        agent.path(),
        cwd.path(),
        &["config", "get", "retry.maxRetries"],
    );
    assert!(ok, "config get after set failed: {stderr}");
    assert!(stdout.contains("= 7  [global]"), "get after set: {stdout}");

    // Enum values validate; a second set replaces the first atomically.
    let (ok, stdout, stderr) = run(agent.path(), cwd.path(), &["config", "set", "transport", "sse"]);
    assert!(ok, "enum set failed: {stderr}");
    assert!(stdout.contains("transport = \"sse\"  [global]"), "enum set: {stdout}");
    let (ok, _stdout, stderr) = run(agent.path(), cwd.path(), &["config", "set", "transport", "udp"]);
    assert!(!ok, "invalid enum must fail");
    assert!(stderr.contains("must be one of"), "enum validation: {stderr}");
    assert_eq!(settings_file(agent.path())["transport"], "sse", "failed set must not persist");

    // Reset clears the scoped layer and falls back to the default.
    let (ok, stdout, stderr) = run(
        agent.path(),
        cwd.path(),
        &["config", "reset", "retry.maxRetries"],
    );
    assert!(ok, "config reset failed: {stderr}");
    assert!(
        stdout.contains("retry.maxRetries = 3  [default]"),
        "reset falls back to the default: {stdout}"
    );
    assert_eq!(settings_file(agent.path()).get("retry"), None, "reset clears the layer");

    // JSON collections round-trip through the typed settings.
    let (ok, _stdout, stderr) = run(
        agent.path(),
        cwd.path(),
        &["config", "set", "extensions", r#"["alpha","beta"]"#],
    );
    assert!(ok, "string-list set failed: {stderr}");
    assert_eq!(settings_file(agent.path())["extensions"], serde_json::json!(["alpha", "beta"]));

    // Secret writes are rejected before any file mutation.
    let (ok, _stdout, stderr) = run(agent.path(), cwd.path(), &["config", "set", "apiKey", "hunter2"]);
    assert!(!ok, "secret write must fail");
    assert!(stderr.contains("secret material"), "{stderr}");
    assert_eq!(settings_file(agent.path()).get("apiKey"), None);
}

#[test]
fn config_project_scope_refuses_untrusted_and_works_approved() {
    let agent = TempDir::new().expect("agent dir");
    let cwd = TempDir::new().expect("cwd");
    // Trust-gated resources make the directory subject to the trust policy:
    // without them the project is implicitly trusted (nothing to gate).
    fs::create_dir_all(cwd.path().join(".pi")).expect(".pi dir");
    fs::write(cwd.path().join(".pi/settings.json"), "{}").expect("seed project settings");

    // Project scope on an untrusted project refuses with an actionable error.
    let (ok, _stdout, stderr) = run(
        agent.path(),
        cwd.path(),
        &["config", "set", "theme", "dark", "--scope", "project"],
    );
    assert!(!ok, "untrusted project set must fail");
    assert!(
        stderr.contains("trusted project"),
        "trust refusal message: {stderr}"
    );

    // --approve trusts the project for this run and persists to .pi.
    let (ok, stdout, stderr) = run(
        agent.path(),
        cwd.path(),
        &["config", "set", "theme", "dark", "--scope", "project", "--approve"],
    );
    assert!(ok, "approved project set failed: {stderr}");
    assert!(stdout.contains("theme = \"dark\"  [project]"), "project set: {stdout}");
    let project = fs::read_to_string(cwd.path().join(".pi/settings.json")).expect("project file");
    let project: Value = serde_json::from_str(&project).expect("project json");
    assert_eq!(project["theme"], "dark");

    // --local selects the project scope for settings verbs too.
    let (ok, _stdout, stderr) = run(
        agent.path(),
        cwd.path(),
        &["config", "--local", "reset", "theme", "--approve"],
    );
    assert!(ok, "approved project reset failed: {stderr}");
    let project = fs::read_to_string(cwd.path().join(".pi/settings.json")).expect("project file");
    let project: Value = serde_json::from_str(&project).expect("project json");
    assert_eq!(project.get("theme"), None, "project reset clears the layer");
}

#[test]
fn bare_config_keeps_the_package_resource_selector() {
    let agent = TempDir::new().expect("agent dir");
    let cwd = TempDir::new().expect("cwd");
    // Piped stdout selects the headless deterministic-JSON path of the
    // package-resource selector (unchanged contract). A directory with no
    // `.pi` resources carries no trust-gated config, so it reports trusted.
    let (ok, stdout, stderr) = run(agent.path(), cwd.path(), &["config"]);
    assert!(ok, "bare config failed: {stderr}");
    let parsed: Value = serde_json::from_str(stdout.trim()).expect("headless stdout is JSON");
    assert_eq!(parsed["scope"].as_str().unwrap(), "global");
    assert_eq!(parsed["projectTrusted"].as_bool().unwrap(), true);
}
