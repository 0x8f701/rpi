use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use tempfile::TempDir;

fn rpi_bin() -> String {
    env!("CARGO_BIN_EXE_rpi").to_owned()
}

fn write_config(agent_dir: &Path, settings: &str, providers: &str) {
    fs::create_dir_all(agent_dir).expect("create agent dir");
    fs::write(agent_dir.join("settings.json"), settings).expect("write settings");
    fs::write(
        agent_dir.join("models.json"),
        format!(r#"{{"providers":{{{providers}}}}}"#),
    )
    .expect("write models");
}

fn provider(id: &str, model: &str, key: Option<&str>) -> String {
    let key = key.map_or(String::new(), |key| format!(r#", "apiKey":"{key}""#));
    format!(
        r#""{id}":{{"baseUrl":"http://localhost:0","api":"openai-completions"{key},"models":[{{"id":"{model}","reasoning":true}}]}}"#
    )
}

fn run(agent_dir: &Path, args: &[&str]) -> (bool, String, String) {
    let output = Command::new(rpi_bin())
        .args(args)
        .env("PI_CODING_AGENT_DIR", agent_dir)
        .env("HOME", agent_dir)
        .env("USERPROFILE", agent_dir)
        .env_remove("PI_PROFILE")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .env_remove("DEEPSEEK_API_KEY")
        .env_remove("GEMINI_API_KEY")
        .env_remove("GROQ_API_KEY")
        .env_remove("OPENROUTER_API_KEY")
        .env_remove("XAI_API_KEY")
        .env_remove("COPILOT_GITHUB_TOKEN")
        .env_remove("ANTHROPIC_AUTH_TOKEN")
        .env_remove("ANTHROPIC_OAUTH_TOKEN")
        .env_remove("ANT_LING_API_KEY")
        .env_remove("QWEN_TOKEN_PLAN_API_KEY")
        .env_remove("QWEN_TOKEN_PLAN_CN_API_KEY")
        .env_remove("AZURE_OPENAI_API_KEY")
        .env_remove("NVIDIA_API_KEY")
        .env_remove("GOOGLE_CLOUD_API_KEY")
        .env_remove("CEREBRAS_API_KEY")
        .env_remove("RADIUS_API_KEY")
        .env_remove("AI_GATEWAY_API_KEY")
        .env_remove("ZAI_API_KEY")
        .env_remove("ZAI_CODING_CN_API_KEY")
        .env_remove("MISTRAL_API_KEY")
        .env_remove("MINIMAX_API_KEY")
        .env_remove("MINIMAX_CN_API_KEY")
        .env_remove("MOONSHOT_API_KEY")
        .env_remove("HF_TOKEN")
        .env_remove("FIREWORKS_API_KEY")
        .env_remove("TOGETHER_API_KEY")
        .env_remove("OPENCODE_API_KEY")
        .env_remove("KIMI_API_KEY")
        .env_remove("CLOUDFLARE_API_KEY")
        .env_remove("XIAOMI_API_KEY")
        .env_remove("XIAOMI_TOKEN_PLAN_CN_API_KEY")
        .env_remove("XIAOMI_TOKEN_PLAN_AMS_API_KEY")
        .env_remove("XIAOMI_TOKEN_PLAN_SGP_API_KEY")
        .env_remove("AWS_PROFILE")
        .env_remove("AWS_ACCESS_KEY_ID")
        .env_remove("AWS_SECRET_ACCESS_KEY")
        .env_remove("AWS_BEARER_TOKEN_BEDROCK")
        .env_remove("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI")
        .env_remove("AWS_CONTAINER_CREDENTIALS_FULL_URI")
        .env_remove("AWS_WEB_IDENTITY_TOKEN_FILE")
        .env_remove("GOOGLE_APPLICATION_CREDENTIALS")
        .env_remove("GCLOUD_PROJECT")
        .env_remove("GOOGLE_CLOUD_PROJECT")
        .stdin(Stdio::null())
        .output()
        .expect("run rpi");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn run_with_env(
    agent_dir: &Path,
    cwd: &Path,
    args: &[&str],
    session_dir_env: Option<&Path>,
) -> (bool, String, String) {
    let mut command = Command::new(rpi_bin());
    command
        .args(args)
        .arg("--cwd")
        .arg(cwd)
        .env("PI_CODING_AGENT_DIR", agent_dir)
        .env("HOME", agent_dir)
        .env("USERPROFILE", agent_dir)
        .env_remove("PI_CODING_AGENT_SESSION_DIR")
        .env_remove("PI_PROFILE")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .env_remove("DEEPSEEK_API_KEY")
        .env("PI_FAUX_RESPONSE", "session-dir-test-response")
        .stdin(Stdio::null());
    if let Some(session_dir) = session_dir_env {
        command.env("PI_CODING_AGENT_SESSION_DIR", session_dir);
    }
    let output = command.output().expect("run rpi");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn run_with_env_stdin(
    agent_dir: &Path,
    cwd: &Path,
    args: &[&str],
    stdin: &str,
) -> (bool, String, String) {
    use std::io::Write as _;

    let mut child = Command::new(rpi_bin())
        .args(args)
        .arg("--cwd")
        .arg(cwd)
        .env("PI_CODING_AGENT_DIR", agent_dir)
        .env("HOME", agent_dir)
        .env("USERPROFILE", agent_dir)
        .env_remove("PI_CODING_AGENT_SESSION_DIR")
        .env_remove("PI_PROFILE")
        .env("PI_FAUX_RESPONSE", "session-dir-test-response")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn rpi");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(stdin.as_bytes())
        .expect("write stdin");
    let output = child.wait_with_output().expect("wait for rpi");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn plant_native_session(directory: &Path, cwd: &Path, id: &str) -> PathBuf {
    fs::create_dir_all(directory).expect("session directory");
    let path = directory.join(format!("{id}.jsonl"));
    let cwd = cwd.display().to_string().replace('\\', "\\\\");
    fs::write(
        &path,
        format!(
            "{{\"type\":\"session\",\"version\":3,\"id\":\"{id}\",\"timestamp\":\"2026-01-01T00:00:00.000Z\",\"cwd\":\"{cwd}\"}}\n\
             {{\"type\":\"message\",\"id\":\"user\",\"parentId\":null,\"timestamp\":\"2026-01-01T00:00:01.000Z\",\"message\":{{\"role\":\"user\",\"content\":\"fixture prompt\"}}}}\n"
        ),
    )
    .expect("session fixture");
    path
}

fn plant_resume(agent_dir: &Path, cwd: &Path, provider: &str, model: &str) -> PathBuf {
    let path = agent_dir.join("resume.jsonl");
    let cwd = cwd.display().to_string().replace('\\', "\\\\");
    let body = format!(
        "{{\"type\":\"session\",\"version\":3,\"id\":\"resume\",\"timestamp\":\"2026-01-01T00:00:00.000Z\",\"cwd\":\"{cwd}\"}}\n\
         {{\"type\":\"model_change\",\"id\":\"model\",\"parentId\":null,\"timestamp\":\"2026-01-01T00:00:00.000Z\",\"provider\":\"{provider}\",\"modelId\":\"{model}\"}}\n\
         {{\"type\":\"thinking_level_change\",\"id\":\"think\",\"parentId\":\"model\",\"timestamp\":\"2026-01-01T00:00:00.000Z\",\"thinkingLevel\":\"low\"}}\n"
    );
    fs::write(&path, body).expect("write resume");
    path
}

#[test]
fn no_flags_honor_settings_default_model() {
    let dir = TempDir::new().unwrap();
    write_config(
        dir.path(),
        r#"{"defaultProvider":"cliproxy","defaultModel":"grok-4.5","defaultThinkingLevel":"high"}"#,
        &provider("cliproxy", "grok-4.5", Some("synthetic-key")),
    );
    let (ok, out, err) = run(dir.path(), &[]);
    assert!(ok, "startup failed: {err}");
    assert!(
        out.contains("cliproxy/grok-4.5"),
        "wrong startup model: {out}"
    );
}

#[test]
fn explicit_cli_model_wins_over_settings() {
    let dir = TempDir::new().unwrap();
    let providers = format!(
        "{},{}",
        provider("cliproxy", "grok-4.5", Some("settings-key")),
        provider("explicit", "chosen", Some("explicit-key"))
    );
    write_config(
        dir.path(),
        r#"{"defaultProvider":"cliproxy","defaultModel":"grok-4.5"}"#,
        &providers,
    );
    let (ok, out, err) = run(dir.path(), &["--model", "explicit/chosen"]);
    assert!(ok, "startup failed: {err}");
    assert!(
        out.contains("explicit/chosen"),
        "CLI model did not win: {out}"
    );
}

#[test]
fn resumed_model_wins_when_cli_model_is_absent() {
    let dir = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    let providers = format!(
        "{},{}",
        provider("cliproxy", "grok-4.5", Some("settings-key")),
        provider("resumed", "saved", Some("resume-key"))
    );
    write_config(
        dir.path(),
        r#"{"defaultProvider":"cliproxy","defaultModel":"grok-4.5"}"#,
        &providers,
    );
    let session = plant_resume(dir.path(), cwd.path(), "resumed", "saved");
    let (ok, out, err) = run(dir.path(), &["--resume", session.to_str().unwrap()]);
    assert!(ok, "startup failed: {err}");
    assert!(
        out.contains("resumed/saved"),
        "resume model did not win: {out}"
    );
}

#[test]
fn unauthenticated_settings_model_falls_back_to_authenticated_model() {
    let dir = TempDir::new().unwrap();
    let providers = format!(
        "{},{}",
        provider("cliproxy", "grok-4.5", None),
        provider("a-backup", "ready", Some("backup-key"))
    );
    write_config(
        dir.path(),
        r#"{"defaultProvider":"cliproxy","defaultModel":"grok-4.5"}"#,
        &providers,
    );
    let (ok, out, err) = run(dir.path(), &[]);
    assert!(ok, "fallback startup failed: {err}");
    assert!(
        out.contains("a-backup/ready"),
        "authenticated fallback not selected: {out}"
    );
}

#[test]
fn invalid_settings_auth_does_not_fall_back_to_another_provider() {
    let dir = TempDir::new().unwrap();
    let providers = format!(
        "{},{}",
        provider("cliproxy", "grok-4.5", Some("$UNSET_SETTINGS_AUTH_KEY")),
        provider("a-backup", "ready", Some("backup-key"))
    );
    write_config(
        dir.path(),
        r#"{"defaultProvider":"cliproxy","defaultModel":"grok-4.5"}"#,
        &providers,
    );
    let (ok, out, err) = run(dir.path(), &[]);
    assert!(!ok, "invalid configured auth must fail closed: {out}");
    assert!(
        err.contains("UNSET_SETTINGS_AUTH_KEY"),
        "missing sanitized cause: {err}"
    );
    assert!(
        !out.contains("a-backup/ready"),
        "prompt route changed providers: {out}"
    );
}

#[test]
fn offline_startup_uses_persisted_radius_model() {
    let dir = TempDir::new().unwrap();
    write_config(
        dir.path(),
        r#"{"defaultProvider":"radius","defaultModel":"offline-radius"}"#,
        "",
    );
    fs::write(
        dir.path().join("auth.json"),
        r#"{"radius":{"type":"api_key","key":"stored-radius-key"}}"#,
    )
    .expect("write Radius auth");
    fs::write(
        dir.path().join("models-store.json"),
        r#"{
  "version": 1,
  "providers": {
    "radius": {
      "models": [{
        "id": "offline-radius",
        "name": "Offline Radius",
        "api": "pi-messages",
        "provider": "radius",
        "baseUrl": "https://radius-stream.example.test/v1",
        "reasoning": true,
        "input": ["text"],
        "cost": {"input": 0.0, "output": 0.0, "cacheRead": 0.0, "cacheWrite": 0.0},
        "contextWindow": 32000,
        "maxTokens": 4096
      }],
      "checkedAt": 42
    }
  }
}"#,
    )
    .expect("write Radius store");

    let (ok, out, err) = run(dir.path(), &["--offline"]);
    assert!(ok, "offline Radius startup failed: {err}");
    assert!(
        out.contains("radius/offline-radius"),
        "persisted Radius model was not selected: {out}"
    );
}

#[test]
fn malformed_settings_error_is_contextual_and_sanitized() {
    let dir = TempDir::new().unwrap();
    let secret = "startup-secret-must-not-leak";
    write_config(
        dir.path(),
        r#"{"defaultProvider": "cliproxy""#,
        &provider("cliproxy", "grok-4.5", Some(secret)),
    );
    let (ok, _, err) = run(dir.path(), &[]);
    assert!(!ok, "malformed settings must fail");
    assert!(
        err.contains("Failed to parse settings.json"),
        "missing context: {err}"
    );
    assert!(!err.contains(secret), "credential leaked in error: {err}");
}


#[test]
fn settings_session_dir_controls_creation_listing_and_continue() {
    let agent = TempDir::new().expect("agent");
    let cwd = TempDir::new().expect("cwd");
    let global_sessions = agent.path().join("global-sessions");
    let sessions = agent.path().join("project-sessions");
    write_config(
        agent.path(),
        &format!(
            r#"{{"sessionDir":{},"defaultProvider":"faux","defaultModel":"faux-1"}}"#,
            serde_json::to_string(&global_sessions).expect("global session dir json")
        ),
        "",
    );
    fs::create_dir_all(cwd.path().join(".pi")).expect("project settings directory");
    fs::write(
        cwd.path().join(".pi/settings.json"),
        format!(r#"{{"sessionDir":{}}}"#, serde_json::to_string(&sessions).expect("project session dir json")),
    )
    .expect("project settings");

    let (created, _, create_err) = run_with_env(
        agent.path(),
        cwd.path(),
        &["--approve", "--offline", "--model", "faux/faux-1", "--session-id", "settings-created", "created"],
        None,
    );
    assert!(created, "settings creation failed: {create_err}");
    assert!(sessions.join("settings-created.jsonl").is_file());
    assert!(!global_sessions.join("settings-created.jsonl").exists());

    let (listed, list_out, list_err) = run_with_env(agent.path(), cwd.path(), &["--approve", "sessions"], None);
    assert!(listed, "settings list failed: {list_err}");
    assert!(list_out.contains("settings-created"), "settings list missed session: {list_out}");

    let (continued, _, continue_err) = run_with_env(
        agent.path(),
        cwd.path(),
        &["--approve", "--offline", "--model", "faux/faux-1", "--continue", "continued"],
        None,
    );
    assert!(continued, "settings continue failed: {continue_err}");
    assert!(continue_err.contains("settings-created.jsonl"), "continued wrong session: {continue_err}");
}

#[test]
fn environment_session_dir_controls_creation_and_listing() {
    let agent = TempDir::new().expect("agent");
    let cwd = TempDir::new().expect("cwd");
    let settings_sessions = agent.path().join("settings-sessions");
    let env_sessions = agent.path().join("env-sessions");
    write_config(
        agent.path(),
        &format!(r#"{{"sessionDir":{}}}"#, serde_json::to_string(&settings_sessions).expect("settings path")),
        "",
    );
    let (created, _, err) = run_with_env(
        agent.path(),
        cwd.path(),
        &["--offline", "--model", "faux/faux-1", "--session-id", "env-created", "created"],
        Some(&env_sessions),
    );
    assert!(created, "environment creation failed: {err}");
    assert!(env_sessions.join("env-created.jsonl").is_file());
    assert!(!settings_sessions.join("env-created.jsonl").exists());

    let (listed, out, list_err) = run_with_env(agent.path(), cwd.path(), &["sessions"], Some(&env_sessions));
    assert!(listed, "environment list failed: {list_err}");
    assert!(out.contains("env-created"), "environment list missed session: {out}");
}

#[test]
fn cli_session_dir_wins_and_empty_environment_is_ignored() {
    let agent = TempDir::new().expect("agent");
    let cwd = TempDir::new().expect("cwd");
    let settings_sessions = agent.path().join("settings-sessions");
    let env_sessions = agent.path().join("env-sessions");
    let cli_sessions = agent.path().join("cli-sessions");
    write_config(
        agent.path(),
        &format!(r#"{{"sessionDir":{}}}"#, serde_json::to_string(&settings_sessions).expect("settings path")),
        "",
    );
    let cli_arg = cli_sessions.to_str().expect("CLI session dir");
    let (created, _, err) = run_with_env(
        agent.path(),
        cwd.path(),
        &["--offline", "--model", "faux/faux-1", "--session-dir", cli_arg, "--session-id", "cli-created", "created"],
        Some(&env_sessions),
    );
    assert!(created, "CLI creation failed: {err}");
    assert!(cli_sessions.join("cli-created.jsonl").is_file());
    assert!(!env_sessions.join("cli-created.jsonl").exists());

    let output = Command::new(rpi_bin())
        .args(["--offline", "--model", "faux/faux-1", "--session-id", "empty-env", "empty"])
        .arg("--cwd")
        .arg(cwd.path())
        .env("PI_CODING_AGENT_DIR", agent.path())
        .env("HOME", agent.path())
        .env("PI_CODING_AGENT_SESSION_DIR", "")
        .env("PI_FAUX_RESPONSE", "session-dir-test-response")
        .stdin(Stdio::null())
        .output()
        .expect("empty environment run");
    assert!(output.status.success(), "empty environment creation failed: {}", String::from_utf8_lossy(&output.stderr));
    assert!(settings_sessions.join("empty-env.jsonl").is_file());
}

#[test]
fn explicit_path_resume_still_opens_that_path() {
    let agent = TempDir::new().expect("agent");
    let cwd = TempDir::new().expect("cwd");
    let selected = agent.path().join("selected-sessions");
    write_config(agent.path(), "{}", "");
    let external = agent.path().join("external");
    let source = plant_native_session(&external, cwd.path(), "explicit-source");
    let source_arg = source.to_str().expect("source path");

    let (resumed, _, resume_err) = run_with_env(
        agent.path(),
        cwd.path(),
        &["--offline", "--model", "faux/faux-1", "--resume", source_arg, "resumed"],
        Some(&selected),
    );
    assert!(resumed, "explicit resume failed: {resume_err}");
    assert!(resume_err.contains(source_arg), "explicit path was not opened: {resume_err}");

    let (opened, _, session_err) = run_with_env(
        agent.path(),
        cwd.path(),
        &["--offline", "--model", "faux/faux-1", "--session", source_arg, "opened"],
        Some(&selected),
    );
    assert!(opened, "explicit --session failed: {session_err}");
    assert!(session_err.contains(source_arg), "explicit --session path was not opened: {session_err}");

    let fork_dir = agent.path().join("fork-output");
    let fork_dir_arg = fork_dir.to_str().expect("fork output path");
    let (forked, _, fork_err) = run_with_env(
        agent.path(),
        cwd.path(),
        &[
            "--offline",
            "--model",
            "faux/faux-1",
            "--session-dir",
            fork_dir_arg,
            "--fork",
            source_arg,
            "forked",
        ],
        Some(&selected),
    );
    assert!(forked, "explicit --fork failed: {fork_err}");
    assert!(
        fs::read_dir(&fork_dir)
            .expect("fork output directory")
            .any(|entry| entry.expect("fork entry").path().extension().is_some_and(|ext| ext == "jsonl")),
        "fork did not record beneath the selected directory"
    );
}

#[test]
fn session_dir_settings_diagnostics_do_not_pollute_json_stdout() {
    let agent = TempDir::new().expect("agent");
    let cwd = TempDir::new().expect("cwd");
    write_config(
        agent.path(),
        r#"{"sessionDir":"sessions","subagents":{"agentOverrides":{"reviewer":{"enabled":false}}}}"#,
        "",
    );
    let (ok, out, err) = run_with_env(
        agent.path(),
        cwd.path(),
        &["--offline", "--mode", "json", "--model", "faux/faux-1", "json"],
        None,
    );
    assert!(ok, "JSON startup failed: {err}");
    assert!(err.contains("deprecated subagents.agentOverrides"), "missing stderr diagnostic: {err}");
    for line in out.lines().filter(|line| !line.trim().is_empty()) {
        serde_json::from_str::<serde_json::Value>(line)
            .unwrap_or_else(|error| panic!("diagnostic polluted JSON stdout ({error}): {line}"));
    }
}

#[test]
fn cli_session_dir_controls_startup_and_interactive_resume_catalogs() {
    let agent = TempDir::new().expect("agent");
    let cwd = TempDir::new().expect("cwd");
    let sessions = agent.path().join("custom-resume-root");
    write_config(agent.path(), "{}", "");
    plant_native_session(&sessions, cwd.path(), "custom-resume");
    let sessions_arg = sessions.to_str().expect("session root");

    let (startup_ok, _, startup_err) = run_with_env(
        agent.path(),
        cwd.path(),
        &[
            "--offline",
            "--model",
            "faux/faux-1",
            "--session-dir",
            sessions_arg,
            "--resume",
            "custom-resume",
            "startup",
        ],
        None,
    );
    assert!(startup_ok, "startup --resume ignored --session-dir: {startup_err}");
    assert!(startup_err.contains("custom-resume.jsonl"), "startup resumed wrong path: {startup_err}");

    let (interactive_ok, interactive_out, interactive_err) = run_with_env_stdin(
        agent.path(),
        cwd.path(),
        &["--offline", "--model", "faux/faux-1", "--session-dir", sessions_arg],
        "/resume custom-resume\n/quit\n",
    );
    assert!(interactive_ok, "interactive /resume failed: {interactive_err}");
    assert!(interactive_out.contains("custom-resume.jsonl"), "interactive /resume ignored stored root: {interactive_out}");
}

#[test]
fn import_session_without_output_uses_effective_session_root() {
    let agent = TempDir::new().expect("agent");
    let cwd = TempDir::new().expect("cwd");
    let sessions = agent.path().join("import-root");
    let source_dir = TempDir::new().expect("source");
    write_config(
        agent.path(),
        &format!(r#"{{"sessionDir":{}}}"#, serde_json::to_string(&sessions).expect("session root")),
        "",
    );
    let source = plant_native_session(source_dir.path(), cwd.path(), "import-source");
    let source_arg = source.to_str().expect("source path");
    let (ok, out, err) = run_with_env(
        agent.path(),
        cwd.path(),
        &["import-session", "pi", source_arg],
        None,
    );
    assert!(ok, "import-session failed: {err}");
    assert!(out.contains(&sessions.display().to_string()), "import ignored effective root: {out}");
}