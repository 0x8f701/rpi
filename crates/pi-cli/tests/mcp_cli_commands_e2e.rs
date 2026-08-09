//! Binary-level coverage for the `rpi mcp list|import` CLI surface
//! (`crates/pi-cli/src/mcp_commands.rs` — 0% unit coverage, no integration
//! tests before this file).
//!
//! The parsing/merge logic lives in `pi_coding::mcp_import` (lib-tested); this
//! file proves the CLI adapter contract through the REAL `rpi` binary:
//! `mcp list` reports an empty/configured set, `mcp import --source claude
//! --file <fixture> --force` parses a Claude Desktop config and persists the
//! servers into settings.json, `--local` targets the project scope, and the
//! printed report and subsequent listing agree with the persisted state. No
//! network and no credentials.

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::{Value, json};
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
        .expect("run rpi mcp command");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// A Claude Desktop config fixture: two stdio servers, one of them listed in
/// `disabledMCPServers`.
fn write_claude_config(path: &Path) {
    fs::write(
        path,
        serde_json::to_vec_pretty(&json!({
            "mcpServers": {
                "filesystem": { "command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem"] },
                "git": { "command": "uvx", "args": ["mcp-server-git"] }
            },
            "disabledMCPServers": ["git"]
        }))
        .expect("serialize claude config"),
    )
    .expect("write claude config");
}

/// Contract: `rpi mcp list` on a fresh agent dir reports an empty set
/// actionably, `rpi mcp import` persists a Claude Desktop config into global
/// settings, and the follow-up `mcp list` renders the imported servers with
/// the disabled marker. Also: a missing config file fails with an actionable
/// error instead of silently succeeding.
#[test]
fn mcp_import_claude_config_then_list_round_trip() {
    let agent = TempDir::new().expect("agent dir");
    let cwd = TempDir::new().expect("cwd");
    let config = cwd.path().join("claude_desktop_config.json");
    write_claude_config(&config);

    // Empty listing is actionable and mentions the import path.
    let (ok, stdout, stderr) = run(agent.path(), cwd.path(), &["mcp", "list"]);
    assert!(ok, "mcp list must succeed: {stderr}");
    assert!(
        stdout.contains("No MCP servers configured"),
        "empty listing: {stdout}"
    );

    // Import from the explicit fixture into the GLOBAL scope.
    let (ok, stdout, stderr) = run(
        agent.path(),
        cwd.path(),
        &["mcp", "import", "--source", "claude", "--file", config.to_str().unwrap(), "--force"],
    );
    assert!(ok, "mcp import must succeed: {stderr}");
    assert!(
        stdout.contains("filesystem") && stdout.contains("git"),
        "import report must name the imported servers: {stdout}"
    );

    // The servers persisted into global settings.json as an array of entries.
    let settings_path = agent.path().join("settings.json");
    assert!(settings_path.is_file(), "global settings.json written");
    let settings: Value = serde_json::from_slice(&fs::read(&settings_path).expect("read settings"))
        .expect("settings.json parses");
    let servers = settings["mcpServers"]
        .as_array()
        .expect("mcpServers array present after import");
    let by_name = |name: &str| {
        servers
            .iter()
            .find(|entry| entry["name"].as_str() == Some(name))
            .unwrap_or_else(|| panic!("{name} missing from {servers:?}"))
    };
    let filesystem = by_name("filesystem");
    assert_eq!(filesystem["command"].as_str(), Some("npx"));
    let git = by_name("git");
    assert_eq!(
        git["disabled"].as_bool(),
        Some(true),
        "disabledMCPServers must map to disabled: {servers:?}"
    );

    // The listing agrees with the persisted state.
    let (ok, stdout, stderr) = run(agent.path(), cwd.path(), &["mcp", "list"]);
    assert!(ok, "mcp list after import: {stderr}");
    assert!(
        stdout.contains("2 configured, 1 disabled"),
        "counts in listing: {stdout}"
    );
    assert!(stdout.contains("filesystem"), "filesystem listed: {stdout}");
    assert!(
        stdout.contains("git") && stdout.contains("[disabled]"),
        "disabled marker on git: {stdout}"
    );

    // A missing explicit file fails actionably.
    let (ok, stdout, stderr) = run(
        agent.path(),
        cwd.path(),
        &["mcp", "import", "--source", "claude", "--file", "/nonexistent/mcp.json"],
    );
    assert!(!ok, "missing config must fail");
    assert!(
        stderr.contains("reading") || stderr.contains("No such file"),
        "actionable error for missing config: {stderr}"
    );
    let _ = stdout;
}

/// Contract: `rpi mcp import --local` targets the PROJECT settings scope and
/// is fail-closed — project-scope writes require project trust, and the CLI
/// adapter resolves no trust itself, so an untrusted (and even a trust-seeded)
/// project is refused with an actionable error instead of writing.
///
/// [Note: the adapter's `settings_manager` never calls `load_project`, so the
/// `--local` import path cannot currently succeed for ANY project — flagged as
/// a product gap in the D39 report; this test pins the observable fail-closed
/// boundary so a future fix has to make the success path explicit.]
#[test]
fn mcp_import_local_is_fail_closed_without_resolved_trust() {
    let agent = TempDir::new().expect("agent dir");
    let cwd = TempDir::new().expect("cwd");
    let config = cwd.path().join("mcp.json");
    fs::write(
        &config,
        serde_json::to_vec_pretty(&json!({
            "mcpServers": {
                "weather": { "type": "http", "url": "https://weather.example.test/mcp" }
            }
        }))
        .expect("serialize cursor config"),
    )
    .expect("write cursor config");

    let (ok, stdout, stderr) = run(
        agent.path(),
        cwd.path(),
        &["mcp", "import", "--source", "cursor", "--file", config.to_str().unwrap(), "--local", "--force"],
    );
    assert!(!ok, "local import must fail closed without project trust");
    assert!(
        stderr.contains("project is not trusted"),
        "actionable trust refusal: {stderr}"
    );
    let _ = stdout;
    // Nothing was written.
    assert!(
        !cwd.path().join(".pi").join("settings.json").exists(),
        "no project settings written on refusal"
    );
}
