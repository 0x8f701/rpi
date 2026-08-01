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
