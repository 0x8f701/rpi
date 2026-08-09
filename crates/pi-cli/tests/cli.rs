//! CLI integration tests that drive the `rpi` binary as a subprocess.
//!
//! Covers: help/version/model and session subcommands, import dispatch, native
//! continuation, and unified native/OMP/Codex/Claude/Grok/Droid resume by path
//! or id/prefix, including ambiguity, malformed/oversized rejection,
//! idempotency, injected homes, and the line REPL catalog surface.
//! All session storage is redirected into per-test temporary homes.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use tempfile::TempDir;

/// Path to the freshly built `rpi` binary (set by cargo for integration tests).
fn rpi_bin() -> String {
    env!("CARGO_BIN_EXE_rpi").to_owned()
}

/// A codex rollout fixture with one user + one assistant message.
fn codex_fixture(dir: &std::path::Path, id: &str) -> std::path::PathBuf {
    let path = dir.join(format!("rollout-{id}.jsonl"));
    let records = [
        r#"{"type":"session_meta","payload":{"id":"c-1","cwd":"<workspace>/pi-rs-test","timestamp":"2025-01-01T00:00:00.000Z"}}"#,
        r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hello there"}]}}"#,
        r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hi right back"}]}}"#,
    ];
    let body = records.join("\n") + "\n";
    fs::write(&path, body).expect("write codex fixture");
    path
}

/// A codex rollout fixture with no convertible messages (meta only).
fn codex_empty_fixture(dir: &std::path::Path, id: &str) -> std::path::PathBuf {
    let path = dir.join(format!("rollout-{id}.jsonl"));
    let body = r#"{"type":"session_meta","payload":{"id":"c-2","cwd":"<workspace>/pi-rs-test","timestamp":"2025-01-01T00:00:00.000Z"}}
"#;
    fs::write(&path, body).expect("write empty codex fixture");
    path
}

fn foreign_fixture(home: &Path, cwd: &Path, source: &str, id: &str) -> PathBuf {
    let cwd = cwd.display().to_string().replace('\\', "\\\\");
    let (path, body) = match source {
        "omp" => (
            home.join(".omp/agent/sessions/project").join(format!("{id}.jsonl")),
            format!(
                "{{\"type\":\"title\",\"v\":1,\"title\":\"OMP fixture\",\"updatedAt\":\"2026-01-01T00:00:00Z\",\"pad\":\" \"}}\n{{\"type\":\"session\",\"version\":3,\"id\":\"{id}\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"cwd\":\"{cwd}\"}}\n{{\"type\":\"message\",\"id\":\"u\",\"parentId\":null,\"message\":{{\"role\":\"user\",\"content\":\"omp question\"}}}}\n{{\"type\":\"message\",\"id\":\"a\",\"parentId\":\"u\",\"message\":{{\"role\":\"assistant\",\"content\":\"omp answer\"}}}}\n"
            ),
        ),
        "codex" => (
            home.join(".codex/sessions").join(format!("rollout-{id}.jsonl")),
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\",\"cwd\":\"{cwd}\"}}}}\n{{\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"user\",\"content\":[{{\"type\":\"input_text\",\"text\":\"codex question\"}}]}}}}\n{{\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{{\"type\":\"output_text\",\"text\":\"codex answer\"}}]}}}}\n"
            ),
        ),
        "claude" => (
            home.join(".claude/projects/project").join(format!("{id}.jsonl")),
            format!(
                "{{\"type\":\"user\",\"uuid\":\"u\",\"parentUuid\":null,\"isSidechain\":false,\"sessionId\":\"{id}\",\"cwd\":\"{cwd}\",\"message\":{{\"role\":\"user\",\"content\":\"claude question\"}}}}\n{{\"type\":\"assistant\",\"uuid\":\"a\",\"parentUuid\":\"u\",\"isSidechain\":false,\"sessionId\":\"{id}\",\"cwd\":\"{cwd}\",\"message\":{{\"role\":\"assistant\",\"content\":\"claude answer\"}}}}\n{{\"type\":\"last-prompt\",\"leafUuid\":\"a\",\"sessionId\":\"{id}\"}}\n"
            ),
        ),
        "grok" => {
            let directory = home.join(".grok/sessions").join(id).join("state");
            let path = directory.join("summary.json");
            fs::create_dir_all(&directory).expect("grok directory");
            fs::write(
                directory.join("chat_history.jsonl"),
                "{\"type\":\"user\",\"content\":\"grok question\"}\n{\"type\":\"assistant\",\"content\":\"grok answer\"}\n",
            )
            .expect("grok chat");
            (
                path,
                format!("{{\"info\":{{\"id\":\"{id}\",\"cwd\":\"{cwd}\"}}}}"),
            )
        }
        "droid" => (
            home.join(".factory/sessions").join(format!("{id}.jsonl")),
            format!(
                "{{\"type\":\"session_start\",\"id\":\"{id}\",\"cwd\":\"{cwd}\"}}\n{{\"type\":\"message\",\"message\":{{\"role\":\"user\",\"content\":\"droid question\"}}}}\n{{\"type\":\"message\",\"message\":{{\"role\":\"assistant\",\"content\":\"droid answer\"}}}}\n"
            ),
        ),
        other => panic!("unsupported fixture source {other}"),
    };
    fs::create_dir_all(path.parent().expect("foreign parent")).expect("foreign parent");
    fs::write(&path, body).expect("foreign fixture");
    path
}

fn command(home: &Path) -> Command {
    let mut command = Command::new(rpi_bin());
    command
        .env("HOME", home)
        .env("SESSIONS_HOME", home)
        .env_remove("PI_CODING_AGENT_DIR")
        .env_remove("PI_PROFILE")
        .env_remove("CODEX_HOME")
        .env_remove("CLAUDE_CONFIG_DIR");
    command
}
fn set_session_import_sources(home: &Path, sources: &[&str]) {
    let agent_dir = home.join(".pi/agent");
    fs::create_dir_all(&agent_dir).expect("agent settings directory");
    fs::write(
        agent_dir.join("settings.json"),
        serde_json::to_vec(&serde_json::json!({ "sessionImportSources": sources }))
            .expect("session import settings"),
    )
    .expect("write session import settings");
}

fn run_stdin(home: &Path, cwd: &Path, args: &[&str], stdin: &str) -> (bool, String, String) {
    use std::io::Write as _;

    let mut child = command(home)
        .args(args)
        .arg("--cwd")
        .arg(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn rpi");
    child
        .stdin
        .as_mut()
        .expect("child stdin")
        .write_all(stdin.as_bytes())
        .expect("write stdin");
    let output = child.wait_with_output().expect("wait for rpi");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn run(home: &std::path::Path, args: &[&str]) -> (bool, String, String) {
    let output = command(home)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("run rpi");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// Encode a cwd the same way `pi_coding::default_session_dir` does so tests
/// can plant session files under the expected on-disk path.
fn encode_cwd_safe_path(path: &Path) -> String {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap().join(path)
    };
    let mut encoded = abs.to_string_lossy().into_owned();
    if encoded.starts_with('/') || encoded.starts_with('\\') {
        encoded.remove(0);
    }
    encoded.replace(['/', '\\', ':'], "-")
}

/// Write a minimal native Pi v3 session under `HOME` for the given `cwd`.
fn plant_session(home: &Path, cwd: &Path, id: &str, timestamp: &str) -> PathBuf {
    plant_session_in(
        &home.join(".pi").join("agent").join("sessions"),
        cwd,
        id,
        timestamp,
    )
}

/// Write a minimal native Pi v3 session under an explicit sessions root (used
/// for profile-scoped fixtures).
fn plant_session_in(sessions_root: &Path, cwd: &Path, id: &str, timestamp: &str) -> PathBuf {
    let dir = sessions_root.join(format!("--{}--", encode_cwd_safe_path(cwd)));
    fs::create_dir_all(&dir).expect("create session dir");
    let path = dir.join(format!(
        "{}_{}.jsonl",
        timestamp.replace([':', '.'], "-"),
        id
    ));
    let cwd_str = if cwd.is_absolute() {
        cwd.display().to_string()
    } else {
        std::env::current_dir()
            .unwrap()
            .join(cwd)
            .display()
            .to_string()
    };
    let body = format!(
        r#"{{"type":"session","version":3,"id":"{id}","timestamp":"{timestamp}","cwd":"{cwd}"}}
{{"type":"message","id":"m1","parentId":null,"timestamp":"{timestamp}","message":{{"role":"user","content":[{{"type":"text","text":"hi"}}],"timestamp":0}}}}
"#,
        id = id,
        timestamp = timestamp,
        cwd = cwd_str.replace('\\', "\\\\"),
    );
    fs::write(&path, body).expect("write session fixture");
    path
}

/// Write a native Pi v3 session with a faux `model_change`, a
/// `thinking_level_change` entry, and a user + assistant message pair under
/// `HOME` for the given `cwd`. The record chain is linear (each `parentId`
/// links to the prior entry) so `build_context` walks the full branch and
/// restores the faux model and recorded thinking level.
fn plant_full_session(home: &Path, cwd: &Path, id: &str, timestamp: &str) -> PathBuf {
    let dir = home
        .join(".pi")
        .join("agent")
        .join("sessions")
        .join(format!("--{}--", encode_cwd_safe_path(cwd)));
    fs::create_dir_all(&dir).expect("create session dir");
    let path = dir.join(format!(
        "{}_{}.jsonl",
        timestamp.replace([':', '.'], "-"),
        id
    ));
    let cwd_str = if cwd.is_absolute() {
        cwd.display().to_string()
    } else {
        std::env::current_dir()
            .unwrap()
            .join(cwd)
            .display()
            .to_string()
    };
    let cwd_escaped = cwd_str.replace('\\', "\\\\");
    // session header → model_change → thinking_level_change → user → assistant.
    let records = [
        format!(
            r#"{{"type":"session","version":3,"id":"{id}","timestamp":"{timestamp}","cwd":"{cwd_escaped}"}}"#
        ),
        format!(
            r#"{{"type":"model_change","id":"mc1","parentId":null,"timestamp":"{timestamp}","provider":"faux","modelId":"faux-1"}}"#
        ),
        format!(
            r#"{{"type":"thinking_level_change","id":"tl1","parentId":"mc1","timestamp":"{timestamp}","thinkingLevel":"high"}}"#
        ),
        format!(
            r#"{{"type":"message","id":"m1","parentId":"tl1","timestamp":"{timestamp}","message":{{"role":"user","content":[{{"type":"text","text":"hello"}}],"timestamp":0}}}}"#
        ),
        format!(
            r#"{{"type":"message","id":"m2","parentId":"m1","timestamp":"{timestamp}","message":{{"role":"assistant","content":[{{"type":"text","text":"hi back"}}],"api":"faux","provider":"faux","model":"faux-1","stopReason":"stop","timestamp":1}}}}"#
        ),
    ];
    let body = records.join("\n") + "\n";
    fs::write(&path, body).expect("write full session fixture");
    path
}

#[test]
fn help_lists_flags_and_subcommands() {
    let home = TempDir::new().unwrap();
    let (ok, out, _) = run(home.path(), &["--help"]);
    assert!(ok, "--help must exit 0");
    for flag in ["--provider", "--system-prompt", "--append-system-prompt", "--add-dir", "--session", "--session-id", "--models", "--tools", "--extension", "--list-models", "--offline", "--profile"] {
        assert!(out.contains(flag), "help lists {flag}");
    }
    for command in ["import-session", "models", "sessions", "install", "remove", "update", "config"] {
        assert!(out.contains(command), "help lists {command}");
    }
}

#[test]
fn version_is_emitted() {
    let home = TempDir::new().unwrap();
    let (ok, out, _) = run(home.path(), &["--version"]);
    assert!(ok, "--version must exit 0");
    assert!(out.starts_with("rpi "), "--version prints \"rpi <version>\"");
}

#[test]
fn models_lists_default_catalog() {
    let home = TempDir::new().unwrap();
    let (ok, out, _) = run(home.path(), &["models"]);
    assert!(ok, "models exits 0");
    assert!(out.contains("faux"), "models include the faux provider");
    assert!(out.contains("faux-1"), "models include the faux-1 model");
}

#[test]
fn models_quarantines_malformed_optional_radius_cache_once_and_stays_quiet() {
    let home = TempDir::new().unwrap();
    let agent_dir = home.path().join(".pi/agent");
    fs::create_dir_all(&agent_dir).expect("create agent directory");
    let store = agent_dir.join("models-store.json");
    fs::write(&store, b"{}").expect("write malformed Radius cache");

    let (ok, out, err) = run(home.path(), &["--offline", "models"]);
    assert!(ok, "models must recover from malformed optional cache: {err}");
    assert!(out.contains("faux-1"), "builtin catalog must remain available: {out}");
    assert!(err.trim().is_empty(), "normal models output must stay quiet: {err:?}");
    assert!(!out.contains("models-store"), "cache diagnostics leaked to stdout: {out}");
    assert!(!store.exists(), "malformed cache must be quarantined");
    assert_eq!(
        fs::read_dir(&agent_dir)
            .expect("read agent directory")
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("models-store.json.invalid-")
            })
            .count(),
        1
    );

    let (ok, out, err) = run(home.path(), &["--offline", "models"]);
    assert!(ok, "subsequent models launch must stay healthy: {err}");
    assert!(out.contains("faux-1"));
    assert!(err.trim().is_empty(), "subsequent launch repeated warning: {err:?}");
}

#[test]
fn models_provider_headers_are_bold() {
    let home = TempDir::new().unwrap();
    let (ok, out, _) = run(home.path(), &["models", "faux"]);
    assert!(ok, "models exits 0");
    assert!(
        out.contains("\u{1b}[1mfaux\u{1b}[0m"),
        "provider header is bold ANSI: {out:?}"
    );
    assert!(
        out.contains("  faux-1"),
        "model id is indented under header"
    );
}

#[test]
fn models_filter_narrows() {
    let home = TempDir::new().unwrap();
    let (ok, out, _) = run(home.path(), &["models", "faux"]);
    assert!(ok, "models <filter> exits 0");
    assert!(out.contains("faux-1"), "filter keeps faux-1");
}

#[test]
fn models_filter_is_case_sensitive() {
    let home = TempDir::new().unwrap();
    // Catalog ids/providers are lowercase ("faux"); an uppercased needle must
    // not match via case-folding (Go `strings.Contains` is case-sensitive).
    let (ok, out, _) = run(home.path(), &["models", "FAUX"]);
    assert!(ok, "models <filter> exits 0 even when empty");
    assert!(
        !out.contains("faux-1"),
        "case-mismatched filter must not keep faux-1: {out:?}"
    );
    assert!(
        !out.contains("No models"),
        "empty filter result must not emit empty-result prose: {out:?}"
    );
    assert!(out.trim().is_empty(), "stdout stays silent: {out:?}");
}

#[test]
fn models_empty_filter_is_silent() {
    let home = TempDir::new().unwrap();
    let (ok, out, _) = run(home.path(), &["models", "definitely-not-a-model-xyz"]);
    assert!(ok, "unmatched filter still exits 0");
    assert!(!out.contains("No models"), "no empty-result prose: {out:?}");
    assert!(out.trim().is_empty(), "stdout silent on no match: {out:?}");
}

#[test]
fn sessions_empty_for_fresh_dir() {
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    let output = Command::new(rpi_bin())
        .arg("sessions")
        .env("HOME", home.path())
        .current_dir(cwd.path())
        .stdin(Stdio::null())
        .output()
        .expect("run rpi sessions");
    assert!(output.status.success(), "sessions exits 0");
    let out = String::from_utf8_lossy(&output.stdout);
    assert!(out.contains("No sessions"), "empty dir reports no sessions");
}

#[test]
fn sessions_honors_global_cwd_flag() {
    let home = TempDir::new().unwrap();
    let target = TempDir::new().unwrap();
    let other = TempDir::new().unwrap();
    let id = "sess-cwd-1";
    let ts = "2026-01-02T03:04:05.000Z";
    plant_session(home.path(), target.path(), id, ts);

    // `--cwd` before the subcommand.
    let (ok, out, err) = run(
        home.path(),
        &["--cwd", target.path().to_str().unwrap(), "sessions"],
    );
    assert!(ok, "sessions --cwd exits 0: {err}");
    assert!(out.contains(id), "lists session for --cwd target: {out}");
    assert!(out.contains(ts), "prints session timestamp: {out}");

    // `-C` after the subcommand (global flag).
    let (ok2, out2, err2) = run(
        home.path(),
        &["sessions", "-C", target.path().to_str().unwrap()],
    );
    assert!(ok2, "sessions -C exits 0: {err2}");
    assert!(out2.contains(id), "lists session for -C target: {out2}");

    // A different cwd must not see the planted session.
    let (ok3, out3, err3) = run(
        home.path(),
        &["--cwd", other.path().to_str().unwrap(), "sessions"],
    );
    assert!(ok3, "sessions other cwd exits 0: {err3}");
    assert!(
        out3.contains("No sessions"),
        "other cwd has no sessions: {out3}"
    );
    assert!(!out3.contains(id), "other cwd must not list target id");
}

#[test]
fn empty_non_tty_prompt_uses_print_mode_and_requires_content() {
    let home = TempDir::new().unwrap();
    let output = Command::new(rpi_bin())
        .args(["-m", "faux/faux-1", ""])
        .env("HOME", home.path())
        .stdin(Stdio::null())
        .output()
        .expect("run rpi with empty prompt");
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "non-TTY empty prompt must fail");
    assert!(
        err.contains("print mode requires a prompt"),
        "non-TTY follows upstream print-mode selection: {err}"
    );
}

#[test]
fn import_session_bogus_source_fails() {
    let home = TempDir::new().unwrap();
    let (ok, _, err) = run(
        home.path(),
        &["import-session", "bogus", "<workspace>/none.jsonl"],
    );
    assert!(!ok, "bogus source must fail");
    assert!(
        err.contains("unsupported source"),
        "error names the unsupported source: {err}"
    );
}

#[test]
fn import_session_codex_fixture_succeeds() {
    let home = TempDir::new().unwrap();
    let fixtures = TempDir::new().unwrap();
    let out_dir = TempDir::new().unwrap();
    let fixture = codex_fixture(fixtures.path(), "imp1");

    let output = Command::new(rpi_bin())
        .args(["import-session", "codex"])
        .arg(&fixture)
        .args(["--output"])
        .arg(out_dir.path())
        .env("HOME", home.path())
        .stdin(Stdio::null())
        .output()
        .expect("run import-session");
    assert!(output.status.success(), "import-session succeeds");
    let out = String::from_utf8_lossy(&output.stdout);
    assert!(out.contains("imported codex"), "reports import: {out}");
    assert!(out.contains("2 messages"), "counts 2 messages: {out}");
    // An emitted session file must appear inside the requested output dir.
    let emitted: Vec<_> = fs::read_dir(out_dir.path())
        .expect("read output dir")
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
        .collect();
    assert!(
        !emitted.is_empty(),
        "an emitted .jsonl exists in output dir"
    );
}

#[test]
fn import_session_no_convertible_fails() {
    let home = TempDir::new().unwrap();
    let fixtures = TempDir::new().unwrap();
    let out_dir = TempDir::new().unwrap();
    let fixture = codex_empty_fixture(fixtures.path(), "empty1");

    let output = Command::new(rpi_bin())
        .args(["import-session", "codex"])
        .arg(&fixture)
        .args(["--output"])
        .arg(out_dir.path())
        .env("HOME", home.path())
        .stdin(Stdio::null())
        .output()
        .expect("run import-session");
    assert!(!output.status.success(), "no-convertible must fail");
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(
        err.contains("no convertible"),
        "error explains no convertible messages: {err}"
    );
}




#[test]
fn unified_resume_honors_sessions_home_for_native_and_foreign() {
    let process_home = TempDir::new().expect("process home");
    let sessions_home = TempDir::new().expect("sessions home");
    let cwd = TempDir::new().expect("cwd");
    set_session_import_sources(process_home.path(), &["droid"]);
    plant_full_session(
        sessions_home.path(),
        cwd.path(),
        "sessions-home-native",
        "2026-07-02T00:00:00.000Z",
    );
    foreign_fixture(
        sessions_home.path(),
        cwd.path(),
        "droid",
        "sessions-home-droid",
    );

    for input in ["sessions-home-native", "sessions-home-droid"] {
        let output = Command::new(rpi_bin())
            .args([
                "-m",
                "faux/faux-1",
                "--cwd",
                cwd.path().to_str().expect("cwd str"),
                "--resume",
                input,
            ])
            .env("HOME", process_home.path())
            .env("SESSIONS_HOME", sessions_home.path())
            .env_remove("PI_CODING_AGENT_DIR")
            .env_remove("CODEX_HOME")
            .env_remove("CLAUDE_CONFIG_DIR")
            .stdin(Stdio::null())
            .output()
            .expect("resume from sessions home");
        assert!(
            output.status.success(),
            "{input} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
#[test]
fn unified_resume_supports_every_source_by_id_or_prefix() {
    for (source, id, input) in [
        ("omp", "omp-resume", "omp-res"),
        ("codex", "codex-resume", "codex-res"),
        ("claude", "claude-resume", "claude-res"),
        ("grok", "grok-resume", "grok-res"),
        ("droid", "droid-resume", "droid-res"),
    ] {
        let home = TempDir::new().expect("home");
        let cwd = TempDir::new().expect("cwd");
        set_session_import_sources(home.path(), &[source]);
        let source_path = foreign_fixture(home.path(), cwd.path(), source, id);
        let inputs = [source_path.to_str().expect("source path"), input];
        for input in inputs {
            let (ok, out, err) = run(
                home.path(),
                &[
                    "-m",
                    "faux/faux-1",
                    "--cwd",
                    cwd.path().to_str().expect("cwd str"),
                    "--resume",
                    input,
                ],
            );
            assert!(ok, "{source} resume {input} failed: {err}");
            assert!(
                err.contains("resumed 2 messages"),
                "{source} history not loaded: {err}"
            );
            assert!(out.contains("faux/faux-1"), "{source} model header: {out}");
        }
    }

    let home = TempDir::new().expect("native home");
    let cwd = TempDir::new().expect("native cwd");
    plant_full_session(
        home.path(),
        cwd.path(),
        "pi-resume",
        "2026-07-01T00:00:00.000Z",
    );
    let (ok, _, err) = run(
        home.path(),
        &[
            "--cwd",
            cwd.path().to_str().expect("cwd str"),
            "--resume",
            "pi-res",
        ],
    );
    assert!(ok, "native prefix resume failed: {err}");
    assert!(err.contains("resumed 2 messages"), "native history: {err}");
}

#[test]
fn unified_resume_accepts_foreign_path_and_reuses_idempotent_import() {
    let home = TempDir::new().expect("home");
    let cwd = TempDir::new().expect("cwd");
    set_session_import_sources(home.path(), &["codex"]);
    let source = foreign_fixture(home.path(), cwd.path(), "codex", "idempotent-path");

    for input in [source.to_str().expect("source path"), "idempotent-path"] {
        let (ok, _, err) = run(
            home.path(),
            &[
                "-m",
                "faux/faux-1",
                "--cwd",
                cwd.path().to_str().expect("cwd str"),
                "--resume",
                input,
            ],
        );
        assert!(ok, "resume {input} failed: {err}");
    }

    let native_root = home.path().join(".pi/agent/sessions");
    let emitted = walk_files(&native_root)
        .into_iter()
        .filter(|path| path.extension().is_some_and(|extension| extension == "jsonl"))
        .collect::<Vec<_>>();
    assert_eq!(emitted.len(), 1, "idempotent resume emitted {emitted:?}");
}

#[test]
fn unified_resume_rejects_ambiguous_malformed_and_oversized_inputs() {
    let home = TempDir::new().expect("home");
    let cwd = TempDir::new().expect("cwd");
    set_session_import_sources(home.path(), &["codex", "claude"]);
    foreign_fixture(home.path(), cwd.path(), "codex", "ambiguous-one");
    foreign_fixture(home.path(), cwd.path(), "claude", "ambiguous-two");
    let (ok, _, err) = run(
        home.path(),
        &[
            "--cwd",
            cwd.path().to_str().expect("cwd str"),
            "--resume",
            "ambiguous",
        ],
    );
    assert!(!ok, "ambiguous prefix must fail");
    assert!(err.contains("ambiguous"), "ambiguity error: {err}");

    let malformed = home.path().join(".codex/sessions/rollout-malformed.jsonl");
    fs::create_dir_all(malformed.parent().expect("malformed parent")).unwrap();
    fs::write(&malformed, "{not-json\n").unwrap();
    let (ok, _, err) = run(
        home.path(),
        &[
            "--cwd",
            cwd.path().to_str().expect("cwd str"),
            "--resume",
            malformed.to_str().expect("malformed path"),
        ],
    );
    assert!(!ok, "malformed foreign input must fail");
    assert!(
        err.contains("invalid") || err.contains("JSON") || err.contains("json"),
        "malformed error: {err}"
    );

    let oversized = home.path().join(".codex/sessions/rollout-oversized.jsonl");
    let file = fs::File::create(&oversized).expect("oversized file");
    file.set_len(65 * 1024 * 1024).expect("oversized length");
    let (ok, _, err) = run(
        home.path(),
        &[
            "--cwd",
            cwd.path().to_str().expect("cwd str"),
            "--resume",
            oversized.to_str().expect("oversized path"),
        ],
    );
    assert!(!ok, "oversized foreign input must fail");
    assert!(err.contains("exceeds import limits"), "oversized error: {err}");
}


#[test]
fn repl_resume_lists_unified_catalog_and_switches_by_id() {
    let home = TempDir::new().expect("home");
    let cwd = TempDir::new().expect("cwd");
    set_session_import_sources(home.path(), &["grok"]);
    foreign_fixture(home.path(), cwd.path(), "grok", "repl-grok");
    let (ok, out, err) = run_stdin(
        home.path(),
        cwd.path(),
        &["-m", "faux/faux-1"],
        "/resume\n/resume repl-grok\n",
    );
    assert!(ok, "REPL resume failed: {err}");
    assert!(out.contains("[grok/hyper"), "unified row missing: {out}");
    assert!(out.contains("repl-grok"), "session id missing: {out}");
    assert!(out.contains("resumed "), "switch result missing: {out}");
}

#[test]
fn automatic_resume_defaults_native_only_and_rejects_foreign_path_and_id() {
    let home = TempDir::new().expect("home");
    let cwd = TempDir::new().expect("cwd");
    let source = foreign_fixture(home.path(), cwd.path(), "omp", "disabled-omp");

    for input in [source.to_str().expect("source path"), "disabled-omp"] {
        let (ok, _, err) = run(
            home.path(),
            &[
                "-m",
                "faux/faux-1",
                "--cwd",
                cwd.path().to_str().expect("cwd str"),
                "--resume",
                input,
            ],
        );
        assert!(!ok, "default policy must reject foreign resume {input}");
        assert!(err.contains("session not found"), "unexpected rejection: {err}");
    }
}

#[test]
fn manual_repl_import_remains_available_with_native_only_policy() {
    let home = TempDir::new().expect("home");
    let cwd = TempDir::new().expect("cwd");
    let source_home = TempDir::new().expect("source home");
    let source = plant_full_session(
        source_home.path(),
        cwd.path(),
        "manual-import",
        "2026-07-03T00:00:00.000Z",
    );

    let input = format!("/import {}\n/quit\n", source.display());
    let (ok, out, err) = run_stdin(
        home.path(),
        cwd.path(),
        &["-m", "faux/faux-1"],
        &input,
    );
    assert!(ok, "manual /import failed under native-only policy: {err}");
    assert!(out.contains("imported and resumed"), "manual import output: {out}");
}

fn walk_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if !root.is_dir() {
        return files;
    }
    for entry in fs::read_dir(root).expect("read directory") {
        let path = entry.expect("directory entry").path();
        if path.is_dir() {
            files.extend(walk_files(&path));
        } else {
            files.push(path);
        }
    }
    files
}
#[test]
fn resume_native_restores_faux_model_and_message_count() {
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    let path = plant_full_session(home.path(), cwd.path(), "r1", "2026-03-01T00:00:00.000Z");

    // No -m override: the faux model must be restored from the session's
    // model_change record rather than defaulting to anthropic.
    let (ok, out, err) = run(home.path(), &["--resume", path.to_str().unwrap()]);
    assert!(ok, "--resume must exit 0: {err}");
    assert!(out.contains("rpi ·"), "REPL header printed: {out}");
    assert!(
        out.contains("faux/faux-1"),
        "REPL header restores the faux model from model_change: {out}"
    );
    assert!(
        err.contains("resumed 2 messages"),
        "stderr reports the resumed message count: {err}"
    );
}

#[test]
fn resume_native_restores_cwd_from_session_header() {
    let home = TempDir::new().unwrap();
    let session_cwd = TempDir::new().unwrap();
    let path = plant_full_session(
        home.path(),
        session_cwd.path(),
        "cwd-restore",
        "2026-03-02T00:00:00.000Z",
    );

    // Run without --cwd: the session header's cwd must override the process
    // cwd so the REPL reflects the directory the session was started in.
    let (ok, out, err) = run(home.path(), &["--resume", path.to_str().unwrap()]);
    assert!(ok, "--resume must exit 0: {err}");
    assert!(
        out.contains(&session_cwd.path().display().to_string()),
        "REPL header shows the session's cwd, not the process cwd: {out}"
    );
}

#[test]
fn resume_native_cwd_flag_overrides_session_header_cwd() {
    let home = TempDir::new().unwrap();
    let session_cwd = TempDir::new().unwrap();
    let override_cwd = TempDir::new().unwrap();
    let path = plant_full_session(
        home.path(),
        session_cwd.path(),
        "cwd-override",
        "2026-03-03T00:00:00.000Z",
    );

    // An explicit --cwd wins over the session header's stored cwd.
    let (ok, out, err) = run(
        home.path(),
        &[
            "--cwd",
            override_cwd.path().to_str().unwrap(),
            "--resume",
            path.to_str().unwrap(),
        ],
    );
    assert!(ok, "--resume --cwd must exit 0: {err}");
    assert!(
        out.contains(&override_cwd.path().display().to_string()),
        "REPL header shows --cwd, not the session's cwd: {out}"
    );
    assert!(
        !out.contains(&session_cwd.path().display().to_string()),
        "session cwd must not leak into the header when --cwd is set: {out}"
    );
}

#[test]
fn continue_latest_selects_newest_session() {
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();

    // Two sessions for the same cwd with distinct timestamps. The newer one
    // (cont-new) must be selected by --continue.
    plant_full_session(
        home.path(),
        cwd.path(),
        "cont-old",
        "2026-01-01T00:00:00.000Z",
    );
    plant_full_session(
        home.path(),
        cwd.path(),
        "cont-new",
        "2026-06-01T00:00:00.000Z",
    );
    // A foreign session is present too; --continue must remain native-only.
    foreign_fixture(home.path(), cwd.path(), "codex", "foreign-newer");

    let (ok, out, err) = run(
        home.path(),
        &["--cwd", cwd.path().to_str().unwrap(), "--continue"],
    );
    assert!(ok, "--continue must exit 0: {err}");
    assert!(
        err.contains("resumed 2 messages"),
        "stderr reports the resumed message count: {err}"
    );
    assert!(
        err.contains("cont-new"),
        "--continue selects the newest session (path contains its id): {err}"
    );
    assert!(
        !err.contains("cont-old"),
        "--continue must not select the older session: {err}"
    );
    assert!(
        out.contains("faux/faux-1"),
        "REPL header restores the faux model: {out}"
    );
}

/// Seed `auth.json` + empty `settings.json` in the agent dir of the given
/// profile (`None` = default profile) with a distinctive provider name.
fn seed_profile_auth(home: &Path, profile: Option<&str>, provider: &str) {
    let agent_dir = match profile {
        Some(name) => home.join(".pi/agent/profiles").join(name),
        None => home.join(".pi/agent"),
    };
    fs::create_dir_all(&agent_dir).expect("create agent dir");
    fs::write(
        agent_dir.join("auth.json"),
        serde_json::to_vec(&serde_json::json!({
            provider: { "type": "api_key", "key": "test-key" },
        }))
        .expect("serialize auth"),
    )
    .expect("write auth");
    fs::write(agent_dir.join("settings.json"), b"{}").expect("write settings");
}

/// Seed a user skill in the agent dir of the given profile (`None` = default
/// profile).
fn seed_profile_skill(home: &Path, profile: Option<&str>, name: &str) {
    let agent_dir = match profile {
        Some(name) => home.join(".pi/agent/profiles").join(name),
        None => home.join(".pi/agent"),
    };
    let dir = agent_dir.join("skills").join(name);
    fs::create_dir_all(&dir).expect("create skill dir");
    fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: profile fixture skill\n---\n# body\n"),
    )
    .expect("write skill");
}

#[test]
fn profile_relocates_agent_config_skills_and_sessions() {
    let home = TempDir::new().unwrap();
    seed_profile_auth(home.path(), None, "default-provider");
    seed_profile_auth(home.path(), Some("work"), "work-provider");
    seed_profile_skill(home.path(), None, "default-skill");
    seed_profile_skill(home.path(), Some("work"), "work-skill");

    // Named profile: doctor resolves the profile-scoped agent dir and auth.
    let (ok, out, err) = run(home.path(), &["--profile", "work", "doctor", "--json"]);
    assert!(ok, "doctor with profile exits 0: {err}");
    assert!(out.contains("profiles/work"), "agent dir is profile-scoped: {out}");
    assert!(out.contains("work-provider"), "auth read from the profile dir: {out}");
    assert!(
        !out.contains("default-provider"),
        "profile must not read the default auth: {out}"
    );
    assert!(
        !out.contains("default-skill"),
        "profile must not load the default skill: {out}"
    );

    // Settings/resource snapshot loads from the profile dir.
    let (ok, out, err) = run(home.path(), &["--profile", "work", "reload"]);
    assert!(ok, "reload with profile exits 0: {err}");
    assert!(out.contains("work-skill"), "profile snapshot lists profile skills: {out}");
    assert!(
        !out.contains("default-skill"),
        "profile snapshot must not list default skills: {out}"
    );

    // Default profile: everything resolves to the unprofiled base.
    let (ok, out, err) = run(home.path(), &["doctor", "--json"]);
    assert!(ok, "doctor exits 0: {err}");
    assert!(out.contains("default-provider"), "default auth read: {out}");
    assert!(
        !out.contains("work-provider"),
        "default doctor must not see profile auth: {out}"
    );
    assert!(
        !out.contains("profiles/"),
        "default paths stay unprofiled: {out}"
    );

    let (ok, out, err) = run(home.path(), &["reload"]);
    assert!(ok, "reload exits 0: {err}");
    assert!(out.contains("default-skill"), "default snapshot lists default skills: {out}");
    assert!(
        !out.contains("work-skill"),
        "default snapshot must not list profile skills: {out}"
    );
}

#[test]
fn profile_default_keeps_default_base() {
    let home = TempDir::new().unwrap();
    seed_profile_auth(home.path(), None, "default-provider");

    let (ok, out, err) = run(home.path(), &["--profile", "default", "doctor", "--json"]);
    assert!(ok, "doctor with --profile default exits 0: {err}");
    assert!(
        !out.contains("profiles/"),
        "--profile default must not relocate: {out}"
    );
    assert!(
        out.contains("default-provider"),
        "--profile default keeps the default auth: {out}"
    );
}

#[test]
fn profile_env_honored_and_cli_flag_wins() {
    let home = TempDir::new().unwrap();
    seed_profile_auth(home.path(), Some("envwork"), "env-provider");
    seed_profile_auth(home.path(), Some("cliwork"), "cli-provider");

    // PI_PROFILE alone selects the profile.
    let output = command(home.path())
        .env("PI_PROFILE", "envwork")
        .args(["doctor", "--json"])
        .stdin(Stdio::null())
        .output()
        .expect("run rpi with PI_PROFILE");
    assert!(output.status.success(), "PI_PROFILE doctor exits 0");
    let out = String::from_utf8_lossy(&output.stdout);
    assert!(out.contains("profiles/envwork"), "PI_PROFILE relocates: {out}");
    assert!(out.contains("env-provider"), "PI_PROFILE auth is read: {out}");

    // The CLI flag wins over the environment.
    let output = command(home.path())
        .env("PI_PROFILE", "envwork")
        .args(["--profile", "cliwork", "doctor", "--json"])
        .stdin(Stdio::null())
        .output()
        .expect("run rpi with flag and env");
    assert!(output.status.success(), "flag+env doctor exits 0");
    let out = String::from_utf8_lossy(&output.stdout);
    assert!(out.contains("profiles/cliwork"), "CLI flag wins over env: {out}");
    assert!(out.contains("cli-provider"), "CLI profile auth is read: {out}");
    assert!(
        !out.contains("env-provider"),
        "env profile must lose to the flag: {out}"
    );
}

#[test]
fn profile_invalid_name_rejected_actionably() {
    let home = TempDir::new().unwrap();

    let (ok, _out, err) = run(home.path(), &["--profile", "bad/name", "doctor"]);
    assert!(!ok, "invalid --profile must fail");
    assert!(err.contains("--profile"), "error names the flag: {err}");
    assert!(
        err.contains("letters, digits"),
        "error explains the allowed charset: {err}"
    );

    let output = command(home.path())
        .env("PI_PROFILE", "bad/name")
        .args(["doctor"])
        .stdin(Stdio::null())
        .output()
        .expect("run rpi with invalid PI_PROFILE");
    assert!(!output.status.success(), "invalid PI_PROFILE must fail");
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(err.contains("profile name"), "error names the profile: {err}");
    assert!(
        err.contains("letters, digits"),
        "error explains the allowed charset: {err}"
    );
}

#[test]
fn profile_scoped_session_listing_is_isolated() {
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    let profile_root = home.path().join(".pi/agent/profiles/work/sessions");
    plant_session_in(
        &profile_root,
        cwd.path(),
        "profiled-session",
        "2026-02-02T02:02:02.000Z",
    );
    plant_session(
        home.path(),
        cwd.path(),
        "default-session",
        "2026-01-01T01:01:01.000Z",
    );

    // The named profile lists only its own sessions, under the profile root.
    let (ok, out, err) = run(
        home.path(),
        &[
            "--profile",
            "work",
            "--cwd",
            cwd.path().to_str().unwrap(),
            "sessions",
        ],
    );
    assert!(ok, "profile sessions exits 0: {err}");
    assert!(out.contains("profiled-session"), "profile lists its session: {out}");
    assert!(out.contains("profiles/work"), "listed path is profile-scoped: {out}");
    assert!(
        !out.contains("default-session"),
        "profile must not see default sessions: {out}"
    );

    // The default profile sees only its own sessions.
    let (ok, out, err) = run(
        home.path(),
        &["--cwd", cwd.path().to_str().unwrap(), "sessions"],
    );
    assert!(ok, "default sessions exits 0: {err}");
    assert!(out.contains("default-session"), "default lists its session: {out}");
    assert!(
        !out.contains("profiled-session"),
        "default must not see profile sessions: {out}"
    );
    assert!(
        !out.contains("profiles/"),
        "default listing stays unprofiled: {out}"
    );
}
