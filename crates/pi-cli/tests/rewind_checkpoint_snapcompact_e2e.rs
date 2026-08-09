//! End-to-end coverage for the session-history slash commands that the
//! in-process dispatch suite (`slash_command_dispatch.rs`) only reaches
//! through the `Application` boundary. These tests drive the REAL `rpi`
//! binary in `--mode text` (the `ReplProbe` harness from `goal_loop_e2e.rs`)
//! and assert the observable REPL surface:
//!
//! * `/rewind` — bare listing picks entries from the live transcript, an
//!   index rewind truncates the session and archives the dropped tail to a
//!   `.rewind-*.jsonl` sidecar next to the session file, and a checkpoint
//!   name rewind (`/rewind <name>`) rolls back to the position `/checkpoint`
//!   marked (T102).
//! * `/compact --snap` and its `/snapcompact` alias (T103) — the
//!   deterministic offline archive: the summary block carries the
//!   `## Snapshot Summary (deterministic archive)` marker (never the faux
//!   LLM text, proving no provider call) and the original tail is preserved
//!   in a `.snapcompact-*.jsonl` sidecar.
//! * `/fresh` (T82) — archives the current transcript and switches to a new
//!   recorder with a different id and session file.
//! * `/queue` surface (T86) — empty listing, `cancel`, and the typed error
//!   for an unknown action.
//! * `/goal pin` / `/goal pins` / `/goal unpin` (T87) — pin listing,
//!   unpin, and the "no pins" empty marker through the real REPL.
//! * Transcript redaction (T83) — a faux reply seeded with credential-shaped
//!   text (`sk-…`, `token=…`) renders as `[REDACTED]` on stdout, never the
//!   raw secret.
//!
//! Everything is deterministic and offline: `PI_FAUX_RESPONSE` seeds the
//! faux provider, a fresh temp HOME isolates credentials/settings, and
//! `--session-dir` pins the session root so sidecar files are locatable.
//! Compaction is configured through the agent-dir `settings.json`
//! (`compaction.snapKeepTurns = 1`) so a two-turn session has an archiveable
//! tail without running eleven prompts.

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

/// Deterministic offline assistant text for every faux turn. Never contains
/// `> ` so the REPL prompt detection stays unambiguous.
const FAUX_RESPONSE: &str = "e2e-rewind-reply";

/// Settings written into the temp agent dir so `/compact --snap` archives a
/// two-turn session (default `snapKeepTurns` is 10).
const SNAPCOMPACT_SETTINGS: &str = r#"{"compaction":{"enabled":true,"reserveTokens":16384,"keepRecentTokens":20000,"snapKeepTurns":1}}"#;

fn rpi_bin() -> String {
    env!("CARGO_BIN_EXE_rpi").to_owned()
}

// ---------------------------------------------------------------------------
// REPL harness: real `rpi` binary, `--mode text`, piped stdio
// (goal_loop_e2e.rs pattern).
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
    /// Spawn `rpi --mode text` with an explicit temp session root so sidecar
    /// files (`.rewind-*`, `.snapcompact-*`) are locatable next to the
    /// session file.
    fn spawn(home: &Path, cwd: &Path, session_dir: &Path, args: &[&str]) -> Self {
        Self::spawn_with_response(home, cwd, session_dir, args, FAUX_RESPONSE)
    }

    /// [`Self::spawn`] with an explicit faux reply (used to seed
    /// credential-shaped text for the redaction contract).
    fn spawn_with_response(
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
        let deadline = Instant::now() + Duration::from_secs(30);
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

/// Seed the agent-dir settings for snap compaction and return the agent dir.
fn seed_agent(home: &Path) -> PathBuf {
    let agent = home.join(".pi").join("agent");
    fs::create_dir_all(&agent).expect("create agent dir");
    fs::write(agent.join("settings.json"), SNAPCOMPACT_SETTINGS).expect("write settings");
    agent
}

/// Parse `<count> messages` from the `/session` status line.
fn session_message_count(output: &str) -> usize {
    let line = output
        .lines()
        .find(|line| line.contains(" messages"))
        .unwrap_or_else(|| panic!("no /session status line in: {output:?}"));
    let count = line
        .split(" · ")
        .nth(1)
        .and_then(|part| part.trim().split(' ').next())
        .expect("message count token");
    count.parse().expect("message count is numeric")
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

/// The entry index of the record whose preview contains `needle`, from the
/// bare `/rewind` listing (the index is the first whitespace-separated token
/// of its line).
fn rewind_index_for(listing: &str, needle: &str) -> usize {
    listing
        .lines()
        .filter(|line| !line.contains("rolls back"))
        .find(|line| line.contains(needle))
        .and_then(|line| line.split_whitespace().next())
        .expect("listing line with needle")
        .parse()
        .expect("rewind index is numeric")
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

// ---------------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------------

/// Contract: after two recorded turns, bare `/rewind` lists every entry with
/// its index and preview; `/rewind <index-of-second-prompt>` keeps the first
/// prompt, drops the tail, and archives the dropped records to exactly one
/// `.rewind-*.jsonl` sidecar whose content carries the dropped prompt.
#[test]
fn repl_rewind_lists_truncates_and_archives_sidecar() {
    let home = TempDir::new().expect("home");
    seed_agent(home.path());
    let cwd = TempDir::new().expect("cwd");
    let session_dir = TempDir::new().expect("session dir");
    let mut repl = ReplProbe::spawn(
        home.path(),
        cwd.path(),
        session_dir.path(),
        &["--mode", "text", "--model", "faux/faux-1"],
    );
    repl.ready();

    let first = repl.command("first rewind prompt");
    assert!(first.contains(FAUX_RESPONSE), "turn must stream: {first}");
    let count_after_first = session_message_count(&repl.command("/session"));
    let second = repl.command("second rewind prompt");
    assert!(second.contains(FAUX_RESPONSE), "turn must stream: {second}");

    let before = repl.command("/session");
    let before_count = session_message_count(&before);
    assert!(before_count > count_after_first, "second turn must grow the transcript");

    let listed = repl.command("/rewind");
    assert!(listed.contains("rolls back"), "{listed}");
    assert!(listed.contains("first rewind prompt"), "{listed}");
    assert!(listed.contains("second rewind prompt"), "{listed}");

    let index = rewind_index_for(&listed, "second rewind prompt");
    let outcome = repl.command(&format!("/rewind {index}"));
    assert!(outcome.contains("rewound to"), "{outcome}");
    assert!(outcome.contains("archived tail to"), "{outcome}");

    let after = repl.command("/session");
    let after_count = session_message_count(&after);
    assert!(
        after_count < before_count,
        "rewind must drop records: {before_count} -> {after_count}: {after}"
    );
    assert_eq!(
        after_count, count_after_first,
        "rewind to the second prompt must restore the post-first-prompt transcript: {after}"
    );

    let session_file = session_file_path(&after);
    let sidecars = sidecars_for(&session_file, ".rewind-");
    assert_eq!(sidecars.len(), 1, "exactly one rewind sidecar: {sidecars:?}");
    let archived = fs::read_to_string(&sidecars[0]).expect("read rewind sidecar");
    assert!(
        archived.contains("second rewind prompt"),
        "archived tail must carry the dropped prompt: {archived}"
    );

    repl.quit();
}

/// Contract: `/checkpoint <name>` marks the current position (reported as
/// "marked at entry <id>"), the bare `/rewind` listing annotates the marker,
/// and `/rewind <name>` rolls the session back to the marked position,
/// restoring the pre-mark message count.
#[test]
fn repl_checkpoint_marks_and_rewinds_by_name() {
    let home = TempDir::new().expect("home");
    seed_agent(home.path());
    let cwd = TempDir::new().expect("cwd");
    let session_dir = TempDir::new().expect("session dir");
    let mut repl = ReplProbe::spawn(
        home.path(),
        cwd.path(),
        session_dir.path(),
        &["--mode", "text", "--model", "faux/faux-1"],
    );
    repl.ready();

    repl.command("checkpoint first prompt");
    let count_after_first = session_message_count(&repl.command("/session"));
    let marked = repl.command("/checkpoint mid");
    assert!(
        marked.contains("marked at entry"),
        "checkpoint must report the marked entry: {marked}"
    );
    repl.command("checkpoint second prompt");

    let listed = repl.command("/rewind");
    assert!(
        listed.contains("[checkpoint mid ->"),
        "listing must annotate the checkpoint: {listed}"
    );

    let outcome = repl.command("/rewind mid");
    assert!(outcome.contains("rewound to checkpoint"), "{outcome}");

    let after = repl.command("/session");
    assert_eq!(
        session_message_count(&after),
        count_after_first,
        "rewind to checkpoint must restore the marked position: {after}"
    );

    let sidecars = sidecars_for(&session_file_path(&after), ".rewind-");
    assert_eq!(sidecars.len(), 1, "checkpoint rewind archives a sidecar: {sidecars:?}");

    repl.quit();
}

/// Contract: `/compact --snap` and `/snapcompact` archive deterministically
/// without an LLM call — the exported transcript carries the
/// `## Snapshot Summary (deterministic archive)` marker (only produced by
/// `build_snapcompact_summary`, never by a provider), the archived region
/// lands in exactly one `.snapcompact-*.jsonl` sidecar, and the summary text
/// itself never contains the faux reply that a summarizer call would have
/// produced.
#[test]
fn repl_compact_snap_archives_without_provider_call() {
    let home = TempDir::new().expect("home");
    seed_agent(home.path());
    let cwd = TempDir::new().expect("cwd");
    let session_dir = TempDir::new().expect("session dir");
    let mut repl = ReplProbe::spawn(
        home.path(),
        cwd.path(),
        session_dir.path(),
        &["--mode", "text", "--model", "faux/faux-1"],
    );
    repl.ready();

    repl.command("first snap prompt");
    repl.command("second snap prompt");

    let compacted = repl.command("/compact --snap");
    assert!(
        compacted.contains("compacted") && compacted.contains("estimated tokens"),
        "/compact --snap must report the token change: {compacted}"
    );

    let session_file = session_file_path(&repl.command("/session"));
    let sidecars = sidecars_for(&session_file, ".snapcompact-");
    assert_eq!(sidecars.len(), 1, "exactly one snapcompact sidecar: {sidecars:?}");
    let archived = fs::read_to_string(&sidecars[0]).expect("read snapcompact sidecar");
    assert!(
        archived.contains("first snap prompt"),
        "sidecar must preserve the archived turn: {archived}"
    );
    assert!(
        !archived.contains("## Snapshot Summary"),
        "sidecar holds raw entries, not the summary: {archived}"
    );

    // Export the live session and prove the deterministic summary replaced
    // the archived region (and that the summary itself is not the faux LLM
    // reply, which would appear if any provider call had run).
    let export = cwd.path().join("snap-export.jsonl");
    let exported_path = repl.command(&format!("/export {}", export.display()));
    assert_eq!(exported_path.trim(), export.display().to_string());
    let exported = fs::read_to_string(&export).expect("read exported jsonl");
    let summary_record = exported
        .lines()
        .find_map(|line| {
            let value: Value = serde_json::from_str(line).ok()?;
            value
                .get("summary")
                .and_then(Value::as_str)
                .filter(|summary| summary.contains("Snapshot Summary"))
                .map(ToOwned::to_owned)
        })
        .expect("exported transcript must carry the deterministic summary record");
    assert!(
        summary_record.contains("Archived"),
        "summary must report deterministic archive statistics: {summary_record}"
    );
    assert!(
        !summary_record.contains(FAUX_RESPONSE),
        "summary must not embed the faux reply that a provider call would produce: {summary_record}"
    );

    // The `/snapcompact` alias runs the same deterministic path.
    repl.command("third snap prompt");
    let aliased = repl.command("/snapcompact");
    assert!(
        aliased.contains("compacted") && aliased.contains("estimated tokens"),
        "/snapcompact must report the token change: {aliased}"
    );
    let sidecars_after_alias = sidecars_for(&session_file, ".snapcompact-");
    assert_eq!(
        sidecars_after_alias.len(),
        2,
        "alias must archive a second sidecar: {sidecars_after_alias:?}"
    );

    repl.quit();
}

/// Contract: `/fresh` archives the current transcript and switches to a new
/// recorder — the session id and session file both change, and the old file
/// remains on disk.
#[test]
fn repl_fresh_starts_new_transcript() {
    let home = TempDir::new().expect("home");
    seed_agent(home.path());
    let cwd = TempDir::new().expect("cwd");
    let session_dir = TempDir::new().expect("session dir");
    let mut repl = ReplProbe::spawn(
        home.path(),
        cwd.path(),
        session_dir.path(),
        &["--mode", "text", "--model", "faux/faux-1"],
    );
    repl.ready();

    repl.command("before fresh");
    let before = repl.command("/session");
    let before_id = before
        .split(" · ")
        .next()
        .expect("session id")
        .to_owned();
    let before_file = session_file_path(&before);
    assert!(before_file.is_file(), "session file exists: {before_file:?}");

    let fresh = repl.command("/fresh");
    assert!(fresh.contains("started a new transcript"), "{fresh}");

    let after = repl.command("/session");
    let after_id = after.split(" · ").next().expect("session id").to_owned();
    let after_file = session_file_path(&after);
    assert_ne!(after_id, before_id, "/fresh must switch session ids");
    assert_ne!(after_file, before_file, "/fresh must open a new session file");
    assert!(
        before_file.is_file(),
        "the archived transcript must stay on disk: {before_file:?}"
    );
    assert!(
        !after_file.exists() || session_message_count(&after) == 0,
        "the new transcript must start empty: {after}"
    );

    repl.quit();
}

/// Contract: the `/queue` REPL surface — empty listing, `cancel` on an empty
/// queue, and a typed error for an unknown action.
#[test]
fn repl_queue_empty_and_cancel_surfaces() {
    let home = TempDir::new().expect("home");
    seed_agent(home.path());
    let cwd = TempDir::new().expect("cwd");
    let session_dir = TempDir::new().expect("session dir");
    let mut repl = ReplProbe::spawn(
        home.path(),
        cwd.path(),
        session_dir.path(),
        &["--mode", "text", "--model", "faux/faux-1"],
    );
    repl.ready();

    let empty = repl.command("/queue");
    assert!(empty.contains("Queue is empty"), "{empty}");
    let cancelled = repl.command("/queue cancel");
    assert!(cancelled.contains("Queue is empty"), "{cancelled}");

    let deadline = Instant::now() + Duration::from_secs(10);
    repl.send_line("/queue bogus");
    let mut saw_error = false;
    while Instant::now() < deadline {
        if repl.stderr_snapshot().contains("unknown action") {
            saw_error = true;
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    assert!(saw_error, "/queue bogus must fail with the typed action error");

    repl.quit();
}

/// Contract: `/goal pin <text>` pins a role-model note on the active goal,
/// `/goal pins` lists it numbered, and `/goal unpin <index>` removes it
/// (listing returns to "no pins").
#[test]
fn repl_goal_pin_lists_and_unpins() {
    let home = TempDir::new().expect("home");
    seed_agent(home.path());
    let cwd = TempDir::new().expect("cwd");
    let session_dir = TempDir::new().expect("session dir");
    let mut repl = ReplProbe::spawn(
        home.path(),
        cwd.path(),
        session_dir.path(),
        &["--mode", "text", "--model", "faux/faux-1"],
    );
    repl.ready();

    let created = repl.command("/goal create pin the release checklist");
    assert!(created.contains("active"), "{created}");
    // Let the auto-started goal-work turn settle so a later resume is
    // deterministic (faux turns are local and fast).
    thread::sleep(Duration::from_secs(2));

    let pinned = repl.command("/goal pin keep the checklist in scope");
    assert!(pinned.contains("active"), "pin keeps the goal active: {pinned}");

    let pins = repl.command("/goal pins");
    assert!(
        pins.contains("1. keep the checklist in scope"),
        "pins listing: {pins}"
    );

    // `/goal pins` renders 1-based, but the command's index is 0-based.
    let unpinned = repl.command("/goal unpin 0");
    assert!(unpinned.contains("active"), "unpin keeps the goal active: {unpinned}");

    let empty = repl.command("/goal pins");
    assert!(empty.contains("no pins"), "unpin must empty the list: {empty}");

    repl.command("/goal drop");
    repl.quit();
}

/// Contract: credential-shaped text in an assistant reply is redacted on the
/// way to stdout — `[REDACTED]` appears and the raw secret never does.
///
/// The faux provider streams the reply in 12-byte deltas and the renderer
/// redacts each delta independently, so the seeded secret is exactly one
/// delta ("token=abc123" is 12 bytes) to stay within a single chunk.
#[test]
fn repl_transcript_redaction_redacts_credential_shapes() {
    let secret_token = "token=abc123";
    let home = TempDir::new().expect("home");
    seed_agent(home.path());
    let cwd = TempDir::new().expect("cwd");
    let session_dir = TempDir::new().expect("session dir");

    let mut repl = ReplProbe::spawn_with_response(
        home.path(),
        cwd.path(),
        session_dir.path(),
        &["--mode", "text", "--model", "faux/faux-1"],
        secret_token,
    );
    repl.ready();

    let reply = repl.command("tell me the secret");
    assert!(
        reply.contains("[REDACTED]"),
        "assistant reply must redact credential shapes: {reply}"
    );
    assert!(
        !reply.contains(secret_token) && !reply.contains("abc123"),
        "raw token leaked: {reply}"
    );

    repl.quit();
}
