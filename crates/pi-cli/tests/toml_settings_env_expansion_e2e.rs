//! End-to-end coverage for TOML settings + environment expansion (T101)
//! through the REAL `rpi` binary.
//!
//! The `pi-coding` settings module unit-tests TOML parse/round-trip, the
//! `settings.toml`-over-`settings.json` sibling preference, and
//! runtime-only env expansion; these tests prove the same contracts reach
//! the binary's startup path:
//!
//! * `sessionDir = "$E2E_TOML_SESSIONS"` — the env-expanded path selects
//!   where `rpi` creates and lists sessions (two different env values yield
//!   two different session roots, which is only possible if the reference
//!   was resolved at runtime rather than used literally).
//! * `settings.toml` wins over a `settings.json` sibling naming another dir.
//! * `defaultModel = "$E2E_TOML_MODEL"` drives the startup model selection —
//!   the REPL header prints `rpi · faux/faux-1 · cwd …`.

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::json;
use tempfile::TempDir;

fn rpi_bin() -> String {
    env!("CARGO_BIN_EXE_rpi").to_owned()
}

/// Run `rpi` with a clean environment, the temp agent dir, and an explicit
/// env map. `args` are the CLI arguments; the process cwd is `cwd`.
fn run_with_env(
    agent_dir: &Path,
    cwd: &Path,
    args: &[&str],
    env: &[(&str, &str)],
) -> (bool, String, String) {
    let mut command = Command::new(rpi_bin());
    command
        .args(args)
        .current_dir(cwd)
        .env("PI_CODING_AGENT_DIR", agent_dir)
        .env("HOME", agent_dir)
        .env("USERPROFILE", agent_dir)
        .env("PI_SKIP_VERSION_CHECK", "1")
        .env("PI_FAUX_RESPONSE", "toml-settings-reply")
        .env_remove("PI_OFFLINE")
        .env_remove("PI_PROFILE")
        .env_remove("PI_CODING_AGENT_SESSION_DIR")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .env_remove("DEEPSEEK_API_KEY")
        .env_remove("GEMINI_API_KEY")
        .env_remove("AWS_ACCESS_KEY_ID")
        .env_remove("AWS_SECRET_ACCESS_KEY")
        .env_remove("GOOGLE_APPLICATION_CREDENTIALS");
    for (name, value) in env {
        command.env(name, value);
    }
    let output = command.stdin(Stdio::null()).output().expect("run rpi");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// Contract: a `settings.toml` with `sessionDir = "$E2E_TOML_SESSIONS"`
/// drives session creation and listing through the REAL binary, and the env
/// reference is expanded at runtime: the same literal setting yields
/// different session roots for different env values.
#[test]
fn toml_settings_session_dir_with_env_expansion_is_honored() {
    let agent = TempDir::new().expect("agent");
    let cwd = TempDir::new().expect("cwd");
    let sessions_a = TempDir::new().expect("sessions a");
    let sessions_b = TempDir::new().expect("sessions b");
    fs::write(
        agent.path().join("settings.toml"),
        "sessionDir = \"$E2E_TOML_SESSIONS\"\n",
    )
    .expect("write toml settings");

    let (created, _, create_err) = run_with_env(
        agent.path(),
        cwd.path(),
        &["--offline", "--model", "faux/faux-1", "--session-id", "toml-a", "created"],
        &[("E2E_TOML_SESSIONS", sessions_a.path().to_str().expect("path a"))],
    );
    assert!(created, "creation through TOML settings failed: {create_err}");
    assert!(
        sessions_a.path().join("toml-a.jsonl").is_file(),
        "session must land in the env-expanded root A"
    );

    let (listed_a, out_a, list_err) = run_with_env(
        agent.path(),
        cwd.path(),
        &["sessions"],
        &[("E2E_TOML_SESSIONS", sessions_a.path().to_str().expect("path a"))],
    );
    assert!(listed_a, "listing with root A failed: {list_err}");
    assert!(out_a.contains("toml-a"), "listing with root A missed the session: {out_a}");

    // The same literal setting with a different env value must resolve to a
    // different root — proof the `$E2E_TOML_SESSIONS` reference was expanded
    // at runtime rather than used verbatim.
    let (listed_b, out_b, list_err_b) = run_with_env(
        agent.path(),
        cwd.path(),
        &["sessions"],
        &[("E2E_TOML_SESSIONS", sessions_b.path().to_str().expect("path b"))],
    );
    assert!(listed_b, "listing with root B failed: {list_err_b}");
    assert!(
        !out_b.contains("toml-a"),
        "root B must not see the root A session: {out_b}"
    );
}

/// Contract: when `settings.toml` and `settings.json` both exist, the TOML
/// sibling wins (name-based preference) — a session created under the TOML
/// env-expanded root never lands in the JSON-named dir.
#[test]
fn toml_settings_wins_over_json_sibling() {
    let agent = TempDir::new().expect("agent");
    let cwd = TempDir::new().expect("cwd");
    let json_sessions = TempDir::new().expect("json sessions");
    let toml_sessions = TempDir::new().expect("toml sessions");
    fs::write(
        agent.path().join("settings.json"),
        serde_json::to_vec(&json!({ "sessionDir": json_sessions.path() })).expect("json settings"),
    )
    .expect("write json settings");
    fs::write(
        agent.path().join("settings.toml"),
        "sessionDir = \"$E2E_TOML_SESSIONS\"\n",
    )
    .expect("write toml settings");

    let (created, _, create_err) = run_with_env(
        agent.path(),
        cwd.path(),
        &["--offline", "--model", "faux/faux-1", "--session-id", "toml-wins", "created"],
        &[("E2E_TOML_SESSIONS", toml_sessions.path().to_str().expect("path"))],
    );
    assert!(created, "creation through TOML settings failed: {create_err}");
    assert!(
        toml_sessions.path().join("toml-wins.jsonl").is_file(),
        "TOML root must receive the session"
    );
    assert!(
        !json_sessions.path().join("toml-wins.jsonl").exists(),
        "JSON sibling must lose the session"
    );
}

/// Contract: `defaultProvider` / `defaultModel` in `settings.toml` with an
/// env-expanded model id drive startup model selection — the REPL header
/// prints the resolved `faux/faux-1`.
#[test]
fn toml_settings_default_model_with_env_expansion_drives_startup() {
    let agent = TempDir::new().expect("agent");
    let cwd = TempDir::new().expect("cwd");
    fs::write(
        agent.path().join("settings.toml"),
        "defaultProvider = \"faux\"\ndefaultModel = \"$E2E_TOML_MODEL\"\n",
    )
    .expect("write toml settings");

    let (ok, out, err) = run_with_env(
        agent.path(),
        cwd.path(),
        &["--offline"],
        &[("E2E_TOML_MODEL", "faux-1")],
    );
    assert!(ok, "startup through TOML settings failed: {err}");
    assert!(
        out.contains("rpi · faux/faux-1"),
        "startup must resolve the env-expanded TOML model: {out}"
    );
}
