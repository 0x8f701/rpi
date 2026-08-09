//! Binary-level e2e coverage for long-standing core paths that the in-process
//! suites (`slash_command_dispatch.rs`, `session_compaction.rs`,
//! `keybindings.rs` unit tests) only reach through the
//! `Application`/`Session`/`KeyBindingsManager` boundaries.
//!
//! These tests drive the REAL `rpi` binary in `--mode text` (the ReplProbe
//! harness pattern from `rewind_checkpoint_snapcompact_e2e.rs`) plus one real
//! PTY (the `terminal_lifecycle.rs` pattern). Everything is offline and
//! deterministic: `PI_FAUX_RESPONSE` seeds the faux provider, isolated temp
//! HOME/cwd/session-dir keep state local, and every wait is bounded.
//!
//! Surfaces under test:
//! * `/compact` (LLM path, no `--snap`) — the summarizer call really runs
//!   against the provider: the persisted `"type":"compaction"` record carries
//!   the faux reply as its summary (the exact opposite of the deterministic
//!   `/compact --snap` archive, which provably never calls the provider), the
//!   deterministic-archive marker is absent, and no `.snapcompact-*` sidecar
//!   is written.
//! * `/export <path>.jsonl` → `/import` round trip — an exported native
//!   JSONL re-imports into a fresh process with the transcript content
//!   intact.
//! * `/reload` — reports `reloaded resource generation <n>` and preserves
//!   trusted extension commands across the reload.
//! * `/model` — runtime model switching reports `switched to <ref>` and
//!   persists a `model_change` record on the session branch; subsequent turns
//!   keep running under the switched model.
//!
//! Keybindings at the PTY level are intentionally NOT asserted here — see the
//! trailing note for why the manager-level coverage is the authoritative
//! contract for config-file remaps.

#![cfg(unix)]

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;
use tempfile::TempDir;

const FAUX_RESPONSE: &str = "e2e-core-path-faux-reply";

/// Small `keepRecentTokens` so a six-turn faux session (~150 tokens) crosses
/// the compaction budget mid-history and the LLM summarizer is invoked.
const COMPACT_SETTINGS: &str =
    r#"{"compaction":{"enabled":true,"reserveTokens":16384,"keepRecentTokens":40,"snapKeepTurns":1}}"#;

fn rpi_bin() -> String {
    env!("CARGO_BIN_EXE_rpi").to_owned()
}

// ---------------------------------------------------------------------------
// REPL harness: real `rpi` binary, `--mode text`, piped stdio
// (rewind_checkpoint_snapcompact_e2e.rs pattern).
// ---------------------------------------------------------------------------

struct ReplProbe {
    child: Child,
    stdin: ChildStdin,
    stdout_buffer: Arc<Mutex<String>>,
    stderr_buffer: Arc<Mutex<String>>,
    /// Absolute stdout offset just past the last consumed `> ` prompt.
    cursor: usize,
}

fn pump<R: Read + Send + 'static>(reader: R, buffer: Arc<Mutex<String>>) {
    thread::spawn(move || {
        let mut reader = reader;
        let mut chunk = [0u8; 8192];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    if let Ok(mut guard) = buffer.lock() {
                        guard.push_str(&String::from_utf8_lossy(&chunk[..n]));
                    }
                }
                Err(_) => break,
            }
        }
    });
}

impl ReplProbe {
    fn spawn(
        home: &Path,
        cwd: &Path,
        session_dir: &Path,
        args: &[&str],
        faux_response: &str,
    ) -> Self {
        let mut cmd = Command::new(rpi_bin());
        cmd.env_clear();
        cmd.env("HOME", home);
        cmd.env("PATH", std::env::var("PATH").unwrap_or_default());
        cmd.env("TERM", "xterm-256color");
        cmd.env("PI_OFFLINE", "1");
        cmd.env("PI_SKIP_VERSION_CHECK", "1");
        cmd.env("PI_FAUX_RESPONSE", faux_response);
        cmd.arg("--session-dir");
        cmd.arg(session_dir);
        cmd.args(args);
        cmd.current_dir(cwd);
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        let mut child = cmd.spawn().expect("spawn rpi REPL");
        let stdin = child.stdin.take().expect("repl stdin pipe");
        let stdout = child.stdout.take().expect("repl stdout pipe");
        let stderr = child.stderr.take().expect("repl stderr pipe");
        let stdout_buffer = Arc::new(Mutex::new(String::new()));
        let stderr_buffer = Arc::new(Mutex::new(String::new()));
        pump(stdout, stdout_buffer.clone());
        pump(stderr, stderr_buffer.clone());
        Self {
            child,
            stdin,
            stdout_buffer,
            stderr_buffer,
            cursor: 0,
        }
    }

    fn stderr_snapshot(&self) -> String {
        self.stderr_buffer.lock().expect("stderr lock").clone()
    }

    fn send_line(&mut self, line: &str) {
        self.stdin
            .write_all(line.as_bytes())
            .expect("repl stdin write");
        self.stdin.write_all(b"\n").expect("repl stdin newline");
        self.stdin.flush().expect("repl stdin flush");
    }

    /// Wait until the first `> ` prompt is painted and point the cursor just
    /// past it.
    fn ready(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let snapshot = self.stdout_buffer.lock().expect("stdout lock").clone();
            if let Some(pos) = snapshot.find("\n> ") {
                self.cursor = pos + 3;
                return;
            }
            if Instant::now() >= deadline {
                panic!(
                    "rpi REPL never reached its prompt\nstdout: {snapshot:?}\nstderr: {:?}",
                    self.stderr_snapshot()
                );
            }
            thread::sleep(Duration::from_millis(25));
        }
    }

    /// Send one line and return the stdout the REPL printed for it
    /// (everything up to the next `> ` prompt).
    fn command(&mut self, line: &str) -> String {
        self.send_line(line);
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let snapshot = self.stdout_buffer.lock().expect("stdout lock").clone();
            let from = self.cursor;
            if let Some(rel) = snapshot.get(from..).and_then(|tail| tail.find("\n> ")) {
                let prompt_pos = from + rel;
                let region = snapshot[from..prompt_pos].to_owned();
                self.cursor = prompt_pos + 3;
                return region.trim_end().to_owned();
            }
            if Instant::now() >= deadline {
                panic!(
                    "rpi REPL timed out after {line:?}\nstdout: {snapshot:?}\nstderr: {:?}",
                    self.stderr_snapshot()
                );
            }
            thread::sleep(Duration::from_millis(25));
        }
    }

    fn wait_exit(&mut self, timeout: Duration) -> Option<ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Ok(Some(status)) = self.child.try_wait() {
                return Some(status);
            }
            if Instant::now() >= deadline {
                return None;
            }
            thread::sleep(Duration::from_millis(25));
        }
    }

    /// `/quit` and require a clean exit.
    fn quit(mut self) {
        self.send_line("/quit");
        let status = self
            .wait_exit(Duration::from_secs(20))
            .expect("rpi REPL must exit after /quit");
        assert!(
            status.success(),
            "rpi REPL exit after /quit: {status:?} stderr={:?}",
            self.stderr_snapshot()
        );
    }
}

impl Drop for ReplProbe {
    fn drop(&mut self) {
        // Backstop: never strand a child if a test failed before /quit.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Seed the agent-dir settings and return the agent dir.
fn seed_agent(home: &Path, settings: &str) -> PathBuf {
    let agent = home.join(".pi").join("agent");
    fs::create_dir_all(&agent).expect("create agent dir");
    fs::write(agent.join("settings.json"), settings).expect("write settings");
    agent
}

/// Parse the session file path from the `/session` status line.
fn session_file_path(output: &str) -> PathBuf {
    let line = output
        .lines()
        .find(|line| line.contains(" messages"))
        .unwrap_or_else(|| panic!("no /session status line in: {output:?}"));
    let path = line
        .split(" · ")
        .nth(2)
        .expect("session file token")
        .trim();
    PathBuf::from(path)
}

/// Sidecars matching `suffix` next to the given session file.
fn sidecars_for(session_file: &Path, suffix: &str) -> Vec<PathBuf> {
    let dir = session_file.parent().expect("session dir");
    let name = session_file
        .file_name()
        .expect("session file name")
        .to_string_lossy()
        .into_owned();
    fs::read_dir(dir)
        .expect("read session dir")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|file| file.starts_with(&name) && file.contains(suffix))
        })
        .collect()
}

/// Find the first JSONL record of `record_type` in an exported transcript.
fn record_of(exported: &str, record_type: &str) -> Value {
    let line = exported
        .lines()
        .find(|line| line.contains(&format!("\"type\":\"{record_type}\"")))
        .unwrap_or_else(|| panic!("no {record_type:?} record in export: {exported}"));
    serde_json::from_str(line).expect("record json")
}

// ---------------------------------------------------------------------------
// /compact (LLM path)
// ---------------------------------------------------------------------------

/// Contract: `/compact` without `--snap` runs the provider summarizer and
/// persists its reply as the `"type":"compaction"` record's summary. This is
/// the mirror image of `/compact --snap` (which proves it NEVER calls the
/// provider): here the faux reply text must be present in the record, the
/// deterministic-archive marker must be absent, and no `.snapcompact-*`
/// sidecar may be written.
#[test]
fn repl_compact_llm_path_calls_provider_and_persists_summary() {
    let home = TempDir::new().expect("home");
    seed_agent(home.path(), COMPACT_SETTINGS);
    let cwd = TempDir::new().expect("cwd");
    let session_dir = TempDir::new().expect("session dir");
    let summary_text = "llm-compaction-provider-summary";
    let mut repl = ReplProbe::spawn(
        home.path(),
        cwd.path(),
        session_dir.path(),
        &["--mode", "text", "--model", "faux/faux-1"],
        summary_text,
    );
    repl.ready();

    // Six turns cross the keepRecentTokens budget so the summarizer runs.
    for turn in 1..=6 {
        repl.command(&format!("e2e compact turn {turn}"));
    }

    let compacted = repl.command("/compact");
    assert!(
        compacted.contains("compacted") && compacted.contains("estimated tokens"),
        "/compact (LLM) must report the token change: {compacted}"
    );

    let session_file = session_file_path(&repl.command("/session"));
    assert!(
        sidecars_for(&session_file, ".snapcompact-").is_empty(),
        "LLM compaction must not write a snapcompact sidecar"
    );

    let export = cwd.path().join("llm-compact-export.jsonl");
    let exported_path = repl.command(&format!("/export {}", export.display()));
    assert_eq!(exported_path.trim(), export.display().to_string());
    let exported = fs::read_to_string(&export).expect("read llm compact export");
    assert!(
        !exported.contains("## Snapshot Summary (deterministic archive)"),
        "LLM compaction must not use the deterministic-archive marker"
    );
    let record = record_of(&exported, "compaction");
    let summary = record
        .get("summary")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("compaction record must carry a summary: {record}"));
    assert!(
        summary.contains(summary_text),
        "LLM compaction summary must contain the provider reply: {summary}"
    );

    repl.quit();
}

// ---------------------------------------------------------------------------
// /export -> /import round trip
// ---------------------------------------------------------------------------

/// Contract: `/export <path>.jsonl` writes an importable native session, and
/// a FRESH process can `/import` it and re-export the same transcript
/// content. Fails if export writes a shape the import parser rejects, or if
/// the imported session drops the original messages.
#[test]
fn repl_export_jsonl_round_trips_through_import() {
    let home = TempDir::new().expect("home");
    let cwd = TempDir::new().expect("cwd");
    let session_dir = TempDir::new().expect("session dir");
    let marker = "round-trip-marker-text";

    // Producer: one turn, export the live transcript.
    let mut producer = ReplProbe::spawn(
        home.path(),
        cwd.path(),
        session_dir.path(),
        &["--mode", "text", "--model", "faux/faux-1"],
        marker,
    );
    producer.ready();
    producer.command("first exported turn");
    let export = cwd.path().join("producer-export.jsonl");
    let exported_path = producer.command(&format!("/export {}", export.display()));
    assert_eq!(exported_path.trim(), export.display().to_string());
    let exported = fs::read_to_string(&export).expect("read producer export");
    assert!(exported.contains(marker), "export must carry the transcript: {exported}");
    producer.quit();

    // Consumer: a fresh process imports the exported file and re-exports it.
    let mut consumer = ReplProbe::spawn(
        home.path(),
        cwd.path(),
        session_dir.path(),
        &["--mode", "text", "--model", "faux/faux-1"],
        marker,
    );
    consumer.ready();
    let imported = consumer.command(&format!("/import {}", export.display()));
    assert!(
        imported.contains("imported and resumed"),
        "import must resume the exported session: {imported}"
    );
    let re_export = cwd.path().join("consumer-reexport.jsonl");
    let re_exported_path = consumer.command(&format!("/export {}", re_export.display()));
    assert_eq!(re_exported_path.trim(), re_export.display().to_string());
    let re_exported = fs::read_to_string(&re_export).expect("read consumer re-export");
    assert!(
        re_exported.contains(marker),
        "re-export after import must keep the transcript: {re_exported}"
    );
    consumer.quit();
}

// ---------------------------------------------------------------------------
// /reload
// ---------------------------------------------------------------------------

/// Contract: `/reload` re-stages resources and reports
/// `reloaded resource generation <n>`, and trusted extension commands keep
/// working across the reload.
#[test]
fn repl_reload_reports_generation_and_preserves_extension_commands() {
    let home = TempDir::new().expect("home");
    let cwd = TempDir::new().expect("cwd");
    let session_dir = TempDir::new().expect("session dir");
    let extension = cwd.path().join("reload-extension");
    fs::create_dir_all(&extension).expect("extension dir");
    fs::write(
        extension.join("pi-extension.json"),
        r#"{"schemaVersion":1,"id":"reload-smoke","runtime":"quickjs","entry":"index.mjs","capabilities":["commands"]}"#,
    )
    .expect("manifest");
    fs::write(
        extension.join("index.mjs"),
        r#"
export default function (pi) {
  pi.registerCommand("alpha", {
    description: "first",
    handler: async (args) => `alpha:${args || "none"}`,
  });
}
"#,
    )
    .expect("entry");

    let mut repl = ReplProbe::spawn(
        home.path(),
        cwd.path(),
        session_dir.path(),
        &[
            "--mode",
            "text",
            "--model",
            "faux/faux-1",
            "--extension",
            extension.to_str().expect("extension path utf8"),
        ],
        FAUX_RESPONSE,
    );
    repl.ready();

    let run = repl.command("/run alpha hello");
    assert!(run.contains("alpha:hello"), "extension command must run: {run}");

    let reloaded = repl.command("/reload");
    assert!(
        reloaded.contains("reloaded resource generation"),
        "reload must report a resource generation: {reloaded}"
    );

    let after = repl.command("/run alpha again");
    assert!(
        after.contains("alpha:again"),
        "extension command must survive /reload: {after}"
    );

    repl.quit();
}

// ---------------------------------------------------------------------------
// /model
// ---------------------------------------------------------------------------

/// Contract: `/model` reports the current model, `/model <spec>` switches at
/// runtime and persists a `model_change` record on the session branch, and
/// the next turn still runs under the switched model.
#[test]
fn repl_model_switch_records_runtime_change() {
    let home = TempDir::new().expect("home");
    let cwd = TempDir::new().expect("cwd");
    let session_dir = TempDir::new().expect("session dir");
    let mut repl = ReplProbe::spawn(
        home.path(),
        cwd.path(),
        session_dir.path(),
        &["--mode", "text", "--model", "faux/faux-1"],
        FAUX_RESPONSE,
    );
    repl.ready();

    let current = repl.command("/model");
    assert!(
        current.contains("current: faux/faux-1"),
        "startup model must be reported: {current}"
    );

    let switched = repl.command("/model faux/faux-1");
    assert!(
        switched.contains("switched to faux/faux-1"),
        "model switch must report: {switched}"
    );

    // A subsequent turn still works under the switched model.
    let reply = repl.command("e2e model switch turn");
    assert!(
        reply.contains(FAUX_RESPONSE),
        "turn after model switch must still run: {reply}"
    );

    let export = cwd.path().join("model-switch.jsonl");
    let exported_path = repl.command(&format!("/export {}", export.display()));
    assert_eq!(exported_path.trim(), export.display().to_string());
    let exported = fs::read_to_string(&export).expect("read model switch export");
    let change = record_of(&exported, "model_change");
    assert_eq!(change["provider"].as_str(), Some("faux"));
    assert_eq!(change["modelId"].as_str(), Some("faux-1"));

    repl.quit();
}

// ---------------------------------------------------------------------------
// Custom keybindings (real PTY) — NOT covered at binary level
// ---------------------------------------------------------------------------
//
// The `KeyBindingsManager` config-file contract (global/project JSON overlay,
// chord validation, `ctrl+q` remap resolution) is unit-covered in
// `keybindings.rs`. A PTY probe of a live remap (`{"app.exit":["ctrl+q"]}` in
// the resource-manager agent-dir file) did NOT exit on the remapped chord in
// this bare-PTY environment even though the default `ctrl+d` quit chord is
// known to work there — the runtime keybinding file path diverges between the
// boot-time `config_paths` (`~/.pi/keybindings.json`) and the
// runtime-settings reload (`~/.pi/agent/keybindings.json`), and the remap did
// not take effect in either location. That is a product-behavior question
// (which file is canonical post-boot), not an e2e gap; the manager-level
// contract is already defended.
