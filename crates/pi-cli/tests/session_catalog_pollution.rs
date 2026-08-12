//! Regression: the real `session_list` surface over an isolated HOME shows
//! exactly the user's sessions — legal short single-turn and image-only
//! sessions are preserved, the Web AllProjects pipeline's backend filter
//! (`filter_web_noise_rows`) hides ONLY empty <10KiB rows, and the
//! recoverable-view partition (`partition_web_noise_rows`) flags the
//! historical test-harness shape (unnamed, tiny, native Pi, cwd under the OS
//! temp root) as `temporary` — never deleted, always recoverable by search /
//! active / loaded, exactly as the sidebar contract requires.
//!
//! Three layers of the same contract:
//!   1. `isolated_home_session_list_reports_user_fixtures`: drives the REAL
//!      `rpi rpc session_list` binary with an isolated HOME + seeded native
//!      sessions and asserts the wire rows carry the user fixtures with the
//!      exact messageCount/size semantics (short single-turn and image-only
//!      preserved; an empty sub-10KiB row reports messageCount==0; a
//!      temp-workspace harness row carries real messages). Current scope is
//!      unfiltered by design and every row is `temporary: false`.
//!   2. `web_all_projects_keeps_temp_rows_and_marks_them_temporary`: runs
//!      the exact AllProjects backend + view partition
//!      (`load_resume_catalog` -> `coalesce_web_import_rows` ->
//!      `filter_web_noise_rows` -> `partition_web_noise_rows`) over the
//!      seeded catalog and asserts the backend hides only the empty row while
//!      the temp-workspace harness row SURVIVES the backend and lands in the
//!      temporary bucket (recoverable), and the legal user rows (named or in
//!      a real workspace) are never temporary.
//!   3. `historical_fixture_rows_covered_by_temp_rule_others_need_user_cleanup`:
//!      replicates the historical producer shapes and records exactly which
//!      rows the temporary rule covers (temp-workspace, unnamed, small native
//!      — recoverable, not deleted), which remain regular (repo/worktree-cwd
//!      or named/large harness rows are indistinguishable from legal sessions
//!      and need explicit user cleanup), and the accepted boundary (a legal
//!      unnamed tiny session recorded in a temp workspace is marked temporary
//!      but remains searchable). Evidence is written to CARGO_TARGET_TMPDIR.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// Binary driver (mirrors rpc_binary.rs' harness): spawn the real `rpi rpc`
// with an isolated HOME and pump LF-delimited JSONL from stdout.
// ---------------------------------------------------------------------------

fn rpc_cmd() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rpi"));
    command.arg("rpc");
    command
}

enum RpcLine {
    Line(String),
    Eof,
    Error(std::io::Error),
}

fn pump_stdout(mut stdout: ChildStdout, tx: &mpsc::Sender<RpcLine>) {
    let mut reader = BufReader::new(stdout);
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => {
                let _ = tx.send(RpcLine::Eof);
                return;
            }
            Ok(_) => {
                if tx.send(RpcLine::Line(line)).is_err() {
                    return;
                }
            }
            Err(error) => {
                let _ = tx.send(RpcLine::Error(error));
                return;
            }
        }
    }
}

struct RpcSession {
    child: Child,
    lines: mpsc::Receiver<RpcLine>,
    _home: tempfile::TempDir,
}

impl RpcSession {
    fn spawn(home: tempfile::TempDir, cwd: &Path) -> Self {
        let mut child = rpc_cmd()
            .args(["--offline", "--model", "faux/faux-1"])
            .env("HOME", home.path())
            .env("USERPROFILE", home.path())
            .env("SESSIONS_HOME", home.path())
            .env_remove("PI_CODING_AGENT_DIR")
            .env("PI_OFFLINE", "1")
            .env("PI_SKIP_VERSION_CHECK", "1")
            .env("PI_FAUX_RESPONSE", "session-catalog-pollution-faux")
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn rpi rpc");
        let (tx, rx) = mpsc::channel();
        let stdout = child.stdout.take().expect("stdout pipe");
        std::thread::spawn(move || pump_stdout(stdout, &tx));
        Self {
            child,
            lines: rx,
            _home: home,
        }
    }

    fn write_line(&mut self, line: &str) {
        let stdin = self.child.stdin.as_mut().expect("stdin pipe");
        stdin
            .write_all(line.as_bytes())
            .expect("write rpc stdin");
        if !line.ends_with('\n') {
            stdin.write_all(b"\n").expect("write rpc LF");
        }
        stdin.flush().expect("flush rpc stdin");
    }

    fn read_json_deadline(&mut self, deadline: std::time::Instant) -> Value {
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                let _ = self.child.kill();
                panic!("timed out waiting for the next JSONL record from rpi rpc");
            }
            match self.lines.recv_timeout(remaining) {
                Ok(RpcLine::Line(line)) => {
                    let trimmed = line.trim_end_matches(['\r', '\n']);
                    if trimmed.is_empty() {
                        continue;
                    }
                    assert!(
                        !trimmed.contains('\u{1b}'),
                        "stdout must not contain ANSI escapes: {trimmed:?}"
                    );
                    return serde_json::from_str(trimmed)
                        .unwrap_or_else(|error| panic!("stdout line is not JSON ({error}): {trimmed}"));
                }
                Ok(RpcLine::Eof) => panic!("rpi rpc stdout closed before the next JSONL record"),
                Ok(RpcLine::Error(error)) => panic!("reading rpi rpc stdout: {error}"),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    panic!("rpi rpc stdout reader thread stopped")
                }
            }
        }
    }

    fn read_until(
        &mut self,
        deadline: std::time::Instant,
        mut pred: impl FnMut(&Value) -> bool,
    ) -> (Vec<Value>, Value) {
        let mut seen = Vec::new();
        loop {
            let value = self.read_json_deadline(deadline);
            if pred(&value) {
                return (seen, value);
            }
            seen.push(value);
        }
    }

    fn finish(mut self) {
        self.close_stdin();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                let _ = self.child.kill();
                panic!("timed out draining rpi rpc stdout");
            }
            match self.lines.recv_timeout(remaining) {
                Ok(RpcLine::Line(_)) | Ok(RpcLine::Eof) | Ok(RpcLine::Error(_)) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        let _ = self.child.wait();
    }

    fn close_stdin(&mut self) {
        drop(self.child.stdin.take());
    }
}

// ---------------------------------------------------------------------------
// Session seeding (version-3 native JSONL, mirroring `start_session_in`).
// ---------------------------------------------------------------------------

fn encoded_cwd(cwd: &Path) -> String {
    let s = cwd.to_string_lossy();
    let stripped = s.strip_prefix('/').unwrap_or(&s);
    format!("--{}--", stripped.replace('/', "-"))
}

fn seed_session(home: &Path, cwd: &Path, id: &str, records: &[Value]) -> PathBuf {
    let dir = home
        .join(".pi")
        .join("agent")
        .join("sessions")
        .join(encoded_cwd(cwd));
    fs_create_dir_all(&dir);
    let target = dir.join(format!("2026-01-01T00:00:00Z_{id}.jsonl"));
    let mut content = String::new();
    for record in records {
        content.push_str(&serde_json::to_string(record).expect("serialize record"));
        content.push('\n');
    }
    fs_write(&target, content);
    target
}

fn header(id: &str, cwd: &Path) -> Value {
    json!({
        "type": "session",
        "version": 3,
        "id": id,
        "timestamp": "2026-01-01T00:00:00Z",
        "cwd": cwd.to_string_lossy(),
    })
}

fn user_message(id: &str, text: &str) -> Value {
    json!({
        "type": "message",
        "id": id,
        "parentId": null,
        "timestamp": "2026-01-01T00:00:01Z",
        "message": {"role": "user", "content": [{"type": "text", "text": text}], "timestamp": 1},
    })
}

fn assistant_message(id: &str, text: &str) -> Value {
    json!({
        "type": "message",
        "id": id,
        "parentId": null,
        "timestamp": "2026-01-01T00:00:02Z",
        "message": {"role": "assistant", "content": [{"type": "text", "text": text}], "timestamp": 2},
    })
}

fn image_message(id: &str) -> Value {
    json!({
        "type": "message",
        "id": id,
        "parentId": null,
        "timestamp": "2026-01-01T00:00:01Z",
        "message": {
            "role": "user",
            "content": [{"type": "image", "data": "aW1n", "mimeType": "image/png"}],
            "timestamp": 1,
        },
    })
}

fn session_info(name: &str) -> Value {
    json!({
        "type": "session_info",
        "id": "si-1",
        "parentId": null,
        "timestamp": "2026-01-01T00:00:01Z",
        "name": name,
    })
}

fn fs_create_dir_all(path: &Path) {
    std::fs::create_dir_all(path).expect("create dir");
}

fn fs_write(path: &Path, content: String) {
    std::fs::write(path, content).expect("write fixture");
}

/// Project workspace used for legal user fixtures: a real (non-temp) path so
/// the temporary-workspace partition never flags the row. Purely lexical —
/// the catalog only reads the header cwd string, the directory never exists.
fn project_cwd() -> PathBuf {
    let cwd = PathBuf::from("<workspace>/projects/my-app");
    assert!(
        !cwd.starts_with(std::env::temp_dir()),
        "project fixture cwd must never resolve under the OS temp root"
    );
    cwd
}

/// Historical test-harness workspace shape: lexically under the OS temp root.
fn tmp_workspace_cwd() -> PathBuf {
    std::env::temp_dir().join("pi-noise-workspace")
}

/// Seed the Current-scope fixtures under the isolated HOME: legal short
/// single-turn, image-only, and normal (>=10KiB) rows plus the empty
/// header-only row and a temp-workspace harness row. Every row uses the
/// spawn `cwd` because the Current scope is cwd-scoped and unfiltered; the
/// sidebar's AllProjects scope partitions its own (cwd-agnostic) seed set.
fn seed_fixtures(home: &Path, cwd: &Path) {
    // Short single-turn (user + assistant, tiny) — legal and preserved.
    seed_session(
        home,
        cwd,
        "user-short",
        &[header("user-short", cwd), user_message("m1", "hi"), assistant_message("m2", "hello")],
    );
    // Image-only (user image block, tiny) — legal and preserved. Carries a
    // session name so the catalog's source+cwd+summary dedupe never collapses
    // it into the equally "(no messages)" noise row.
    seed_session(
        home,
        cwd,
        "user-image",
        &[
            header("user-image", cwd),
            session_info("screenshot notes"),
            image_message("m1"),
        ],
    );
    // Normal session with aggregate size above the 10KiB noise threshold.
    seed_session(
        home,
        cwd,
        "user-normal",
        &[
            header("user-normal", cwd),
            user_message("m1", "a real conversation"),
            assistant_message("m2", &format!("answer {}", "y".repeat(16 * 1024))),
        ],
    );
    // Empty row: header only, no messages, small — hidden by the Web
    // AllProjects zero-message rule (Current scope is unfiltered by design,
    // so it still appears here).
    seed_session(home, cwd, "noise-empty", &[header("noise-empty", cwd)]);
    // Historical harness shape: unnamed, tiny, cwd under the OS temp root,
    // but carrying REAL messages — marked temporary by the Web AllProjects
    // partition, never deleted. Still listed in Current scope.
    seed_session(
        home,
        cwd,
        "noise-tmp",
        &[
            header("noise-tmp", cwd),
            user_message("m1", "harness probe"),
            assistant_message("m2", "harness reply"),
        ],
    );
}

/// Seed the AllProjects fixtures: legal rows live in a real project workspace
/// (never the OS temp root); noise rows are an empty header-only row and a
/// small unnamed temp-workspace row with real messages.
fn seed_all_projects_fixtures(home: &Path) {
    let project = project_cwd();
    // Short single-turn (user + assistant, tiny) — legal and preserved.
    seed_session(
        home,
        &project,
        "user-short",
        &[header("user-short", &project), user_message("m1", "hi"), assistant_message("m2", "hello")],
    );
    // Image-only (user image block, tiny) — legal and preserved. Carries a
    // session name so the catalog's source+cwd+summary dedupe never collapses
    // it into the equally "(no messages)" noise row.
    seed_session(
        home,
        &project,
        "user-image",
        &[
            header("user-image", &project),
            session_info("screenshot notes"),
            image_message("m1"),
        ],
    );
    // Normal session with aggregate size above the 10KiB noise threshold.
    seed_session(
        home,
        &project,
        "user-normal",
        &[
            header("user-normal", &project),
            user_message("m1", "a real conversation"),
            assistant_message("m2", &format!("answer {}", "y".repeat(16 * 1024))),
        ],
    );
    // Empty row: header only, no messages, small — hidden by the zero-message rule.
    seed_session(home, &project, "noise-empty", &[header("noise-empty", &project)]);
    // Historical harness shape: unnamed, tiny, cwd under the OS temp root,
    // with real messages — survives the backend, marked temporary.
    let tmp = tmp_workspace_cwd();
    seed_session(
        home,
        &tmp,
        "noise-tmp",
        &[
            header("noise-tmp", &tmp),
            user_message("m1", "harness probe"),
            assistant_message("m2", "harness reply"),
        ],
    );
}

// ---------------------------------------------------------------------------
// Contract 1: the REAL `rpi rpc session_list` (current scope) over an
// isolated HOME reports the user fixtures with noise-compatible semantics,
// and every Current-scope row is `temporary: false`.
// ---------------------------------------------------------------------------

#[test]
fn isolated_home_session_list_reports_user_fixtures() {
    let home = tempfile::tempdir().expect("isolated HOME");
    let cwd = tempfile::tempdir().expect("project cwd");
    seed_fixtures(home.path(), cwd.path());

    let mut session = RpcSession::spawn(home, cwd.path());
    session.write_line(r#"{"type":"session_list","id":"list1"}"#);
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let (_, response) = session.read_until(deadline, |line| {
        line.get("type").and_then(Value::as_str) == Some("response")
            && line.get("id").and_then(Value::as_str) == Some("list1")
    });
    assert_eq!(response["success"], true, "session_list must succeed: {response}");
    assert_eq!(response["command"], "session_list", "command field: {response}");
    let rows = response["data"]["sessions"]
        .as_array()
        .expect("sessions array")
        .clone();

    let by_id = |id: &str| rows.iter().find(|row| row["sessionId"] == id).cloned();

    // Legal short single-turn is preserved in the real catalog.
    let short = by_id("user-short").unwrap_or_else(|| panic!("user-short missing: {rows:?}"));
    assert_eq!(short["source"], "pi", "native source label: {short}");
    assert!(
        short["messageCount"].as_u64().unwrap_or(0) >= 1,
        "short single-turn must carry >=1 message: {short}"
    );
    assert!(
        short["size"].as_u64().unwrap_or(u64::MAX) < 10 * 1024,
        "short single-turn is under the noise threshold (kept by messageCount): {short}"
    );
    assert_eq!(
        short["temporary"], false,
        "Current scope rows must never be temporary: {short}"
    );

    // Image-only session is preserved and counted as exactly one message.
    let image = by_id("user-image").unwrap_or_else(|| panic!("user-image missing: {rows:?}"));
    assert_eq!(
        image["messageCount"].as_u64(),
        Some(1),
        "image-only session must count exactly one message: {image}"
    );
    assert_eq!(
        image["temporary"], false,
        "image-only session must not be temporary: {image}"
    );

    // The normal user session is listed.
    let normal = by_id("user-normal").unwrap_or_else(|| panic!("user-normal missing: {rows:?}"));
    assert!(
        normal["size"].as_u64().unwrap_or(0) >= 10 * 1024,
        "normal fixture must exceed the noise threshold: {normal}"
    );
    assert_eq!(
        normal["temporary"], false,
        "normal session must not be temporary: {normal}"
    );

    // The empty row carries the exact semantics the Web AllProjects pipeline
    // keys on: messageCount == 0 AND size < 10KiB (the backend hides it).
    let noise = by_id("noise-empty").unwrap_or_else(|| panic!("noise-empty missing: {rows:?}"));
    assert_eq!(
        noise["messageCount"].as_u64(),
        Some(0),
        "empty header-only row must report messageCount 0: {noise}"
    );
    assert!(
        noise["size"].as_u64().unwrap_or(u64::MAX) < 10 * 1024,
        "empty row must be under the noise threshold: {noise}"
    );

    // The temp-workspace harness row carries REAL messages and stays under
    // 10KiB, so the Web AllProjects temporary partition is the only thing
    // that flags it — and Current scope must NOT flag it: Current scope
    // never applies the AllProjects partition (rows are temporary: false).
    let harness = by_id("noise-tmp").unwrap_or_else(|| panic!("noise-tmp missing: {rows:?}"));
    assert_eq!(harness["source"], "pi", "native source label: {harness}");
    assert!(
        harness["messageCount"].as_u64().unwrap_or(0) >= 1,
        "temp harness row must carry >=1 message: {harness}"
    );
    assert!(
        harness["size"].as_u64().unwrap_or(u64::MAX) < 10 * 1024,
        "temp harness row must be under the noise threshold: {harness}"
    );
    assert_eq!(
        harness["temporary"], false,
        "Current scope rows must never be flagged temporary: {harness}"
    );

    session.finish();
}

// ---------------------------------------------------------------------------
// Contract 2: the exact Web AllProjects sidebar pipeline
// (load_resume_catalog -> coalesce_web_import_rows -> filter_web_noise_rows
// -> partition_web_noise_rows) keeps user fixtures, hides ONLY the empty
// sub-10KiB row, and marks the temp-workspace harness row temporary.
// ---------------------------------------------------------------------------

#[test]
fn web_all_projects_keeps_temp_rows_and_marks_them_temporary() {
    use pi_cli::resume_catalog::{
        coalesce_web_import_rows, filter_web_noise_rows, load_resume_catalog,
        partition_web_noise_rows, ResumeCatalogRequest,
    };
    use pi_coding::{CatalogSort, SessionCatalog, SessionSourceKind};

    let home = tempfile::tempdir().expect("isolated HOME");
    seed_all_projects_fixtures(home.path());

    // `SessionCatalog::new(home)` roots the native agent dir at
    // `<home>/.pi/agent` (mirroring the default tree) — pass the HOME root,
    // never `<home>/.pi/agent` (that would double the suffix).
    let catalog = SessionCatalog::new(home.path());
    let rows = load_resume_catalog(
        &catalog,
        &ResumeCatalogRequest {
            sources: vec![
                SessionSourceKind::Omp,
                SessionSourceKind::Codex,
                SessionSourceKind::Grok,
            ],
            include_foreign: true,
            dedupe: true,
            named_only: false,
            cwd_scope: None,
            sort: CatalogSort::Newest,
            ..ResumeCatalogRequest::default()
        },
    )
    .expect("catalog list")
    .rows;
    assert_eq!(rows.len(), 5, "catalog must see all five seeds: {rows:?}");

    // Backend: coalesce imported duplicates, then the zero-message filter.
    // The temp-workspace harness row carries real messages, so it SURVIVES
    // the backend (it is only partitioned as temporary, never deleted).
    let backend_rows = filter_web_noise_rows(coalesce_web_import_rows(rows));
    let mut backend_ids: Vec<&str> = backend_rows
        .iter()
        .map(|row| row.session_id.as_str())
        .collect();
    backend_ids.sort_unstable();
    assert_eq!(
        backend_ids,
        vec!["noise-tmp", "user-image", "user-normal", "user-short"],
        "backend must keep the temp harness row (it has real messages) and hide only the empty row"
    );
    assert!(
        !backend_rows
            .iter()
            .any(|row| row.session_id == "noise-empty"),
        "empty sub-10KiB row must be hidden by filter_web_noise_rows"
    );

    // View partition: the temp-workspace harness row lands in the temporary
    // bucket (recoverable — searchable / active / loaded); the legal user
    // rows (real workspace, named image session, or >=10KiB) are regular.
    let (regular, temporary) = partition_web_noise_rows(backend_rows);
    let mut regular_ids: Vec<&str> = regular.iter().map(|row| row.session_id.as_str()).collect();
    regular_ids.sort_unstable();
    let temp_ids: Vec<&str> = temporary.iter().map(|row| row.session_id.as_str()).collect();
    assert_eq!(
        regular_ids,
        vec!["user-image", "user-normal", "user-short"],
        "legal user rows must be regular (never temporary): {regular_ids:?}"
    );
    assert_eq!(
        temp_ids,
        vec!["noise-tmp"],
        "the temp-workspace harness row must be the only temporary row: {temp_ids:?}"
    );
    assert!(
        temporary.iter().all(|row| row.message_count.is_some()),
        "temporary rows still carry their real messages (recoverable, never lost): {temporary:?}"
    );
}

// ---------------------------------------------------------------------------
// Contract 3: which historical fixture rows the temporary rule covers, which
// need user cleanup, and the accepted boundary. Replicates the historical
// producer shapes (workflow supervisor/child sessions in temp worktree cwds,
// clone/rewind rows in real-looking cwds) — fully synthetic, never the real
// HOME. Evidence is written to CARGO_TARGET_TMPDIR so the regression lane can
// cite it.
// ---------------------------------------------------------------------------

#[test]
fn historical_fixture_rows_covered_by_temp_rule_others_need_user_cleanup() {
    use pi_cli::resume_catalog::{
        coalesce_web_import_rows, filter_web_noise_rows, load_resume_catalog,
        partition_web_noise_rows, ResumeCatalogRequest,
    };
    use pi_coding::{CatalogSort, SessionCatalog, SessionSourceKind};

    let home = tempfile::tempdir().expect("isolated HOME");
    let worktree = tmp_workspace_cwd().join("workflow-worktrees").join("wf-0001");
    let user_cwd = project_cwd();
    fs_create_dir_all(&worktree);
    fs_create_dir_all(&user_cwd);

    let tool_result = |id: &str, text: &str| {
        json!({"type":"message","id":id,"parentId":null,"timestamp":"2026-01-01T00:00:03Z",
               "message":{"role":"tool","toolCallId":id,"content":[{"type":"text","text":text}],"timestamp":3}})
    };
    let tool_call = |id: &str, name: &str, args: Value| {
        json!({"type":"message","id":id,"parentId":null,"timestamp":"2026-01-01T00:00:02Z",
               "message":{"role":"assistant","content":[{"type":"toolCall","toolCall":{"id":id,"name":name,"arguments":args}}],"timestamp":2}})
    };
    let model_change = |provider: &str, model: &str| {
        json!({"type":"model_change","id":"mc-1","parentId":null,"timestamp":"2026-01-01T00:00:01Z",
               "modelId":model,"provider":provider})
    };
    let header_with = |id: &str, cwd: &Path, parent: Option<&str>| {
        let mut h = json!({
            "type":"session","version":3,"id":id,"timestamp":"2026-01-01T00:00:00Z",
            "cwd": cwd.to_string_lossy(),
        });
        if let Some(p) = parent {
            h["parentSession"] = json!(p);
        }
        h
    };

    // ---- Historical fixture-shaped rows (replicating the producers) ----
    // Workflow supervisor: temp worktree cwd, faux provider, unnamed, tiny —
    // covered by the temporary rule (recoverable).
    seed_session(
        home.path(),
        &worktree,
        "fx-workflow-supervisor",
        &[
            header_with("fx-workflow-supervisor", &worktree, None),
            model_change("workflow-pollution-provider-abc", "workflow-pollution-abc"),
            user_message("m1", "You plan workflow 019ff1bf-... Objective: ship the feature"),
            tool_call("t1", "todo", json!({"op":"init","items":["create hello.txt"]})),
            tool_result("t1", "Remaining items (1):\n  - create hello.txt [in_progress]"),
            tool_call("t2", "bash", json!({"command":"printf 'hello' > hello.txt && git commit -m x"})),
            tool_result("t2", "ok"),
            assistant_message("m2", "worker one done"),
        ],
    );
    // Durable child: parentSession lineage, temp worktree cwd, unnamed, tiny —
    // covered by the temporary rule.
    seed_session(
        home.path(),
        &worktree,
        "fx-workflow-child",
        &[
            header_with("fx-workflow-child", &worktree, Some("fx-workflow-supervisor")),
            model_change("workflow-pollution-provider-abc", "workflow-pollution-abc"),
            user_message("m1", "complete workflow Todo"),
            assistant_message("m2", "done"),
        ],
    );
    // Clone row: NAMED (clone of ...) in a real workspace — NOT covered by the
    // temporary rule; indistinguishable from a legal session -> user cleanup.
    seed_session(
        home.path(),
        &user_cwd,
        "fx-clone-row",
        &[
            header_with("fx-clone-row", &user_cwd, Some("parent-session-x")),
            session_info("clone of parent-session-x"),
            user_message("m1", "clone"),
            assistant_message("m2", "cloned"),
        ],
    );
    // Rewind/compaction row: unnamed but >=10KiB in a real workspace — NOT
    // covered by the temporary rule -> user cleanup.
    seed_session(
        home.path(),
        &user_cwd,
        "fx-rewind-row",
        &[
            header_with("fx-rewind-row", &user_cwd, None),
            user_message("m1", "rewind me"),
            assistant_message("m2", &format!("compacted {}", "y".repeat(12 * 1024))),
        ],
    );

    // ---- Legal user rows that MUST be preserved as regular ----
    // Short single-turn in a plain project.
    seed_session(
        home.path(),
        &user_cwd,
        "user-short",
        &[
            header_with("user-short", &user_cwd, None),
            user_message("m1", "hi"),
            assistant_message("m2", "hello"),
        ],
    );
    // Image-only session.
    seed_session(
        home.path(),
        &user_cwd,
        "user-image",
        &[
            header_with("user-image", &user_cwd, None),
            image_message("m1"),
        ],
    );
    // Legal user session NAMED like a fixture ("ship the release").
    seed_session(
        home.path(),
        &user_cwd,
        "user-named-ship",
        &[
            header_with("user-named-ship", &user_cwd, None),
            session_info("ship the release"),
            user_message("m1", "plan the release"),
            assistant_message("m2", "release plan ready"),
        ],
    );
    // Legal FORK (parentSession lineage is a real user action).
    seed_session(
        home.path(),
        &user_cwd,
        "user-fork",
        &[
            header_with("user-fork", &user_cwd, Some("user-short")),
            user_message("m1", "forked branch work"),
            assistant_message("m2", "done"),
        ],
    );
    // Legal user with a custom provider whose name looks fixture-ish.
    seed_session(
        home.path(),
        &user_cwd,
        "user-custom-provider",
        &[
            header_with("user-custom-provider", &user_cwd, None),
            model_change("local/workflow-helper", "helper-1"),
            user_message("m1", "use my helper"),
            assistant_message("m2", &format!("result {}", "z".repeat(11 * 1024))),
        ],
    );
    // Accepted boundary: a LEGAL user working in the OS temp root with an
    // unnamed tiny session is marked temporary (hidden by default) but stays
    // recoverable via search / active / loaded — never deleted.
    seed_session(
        home.path(),
        &user_cwd,
        "user-tmp-experiment",
        &[
            header_with("user-tmp-experiment", &tmp_workspace_cwd(), None),
            user_message("m1", "quick experiment"),
            assistant_message("m2", "here is the result"),
        ],
    );

    let catalog = SessionCatalog::new(home.path());
    let rows = load_resume_catalog(
        &catalog,
        &ResumeCatalogRequest {
            sources: vec![
                SessionSourceKind::Omp,
                SessionSourceKind::Codex,
                SessionSourceKind::Grok,
            ],
            include_foreign: true,
            dedupe: true,
            named_only: false,
            cwd_scope: None,
            sort: CatalogSort::Newest,
            ..ResumeCatalogRequest::default()
        },
    )
    .expect("catalog list")
    .rows;

    let fixture_ids = [
        "fx-workflow-supervisor",
        "fx-workflow-child",
        "fx-clone-row",
        "fx-rewind-row",
    ];
    let legal_ids = [
        "user-short",
        "user-image",
        "user-named-ship",
        "user-fork",
        "user-custom-provider",
        "user-tmp-experiment",
    ];
    assert_eq!(rows.len(), 10, "catalog must see all ten seeds: {rows:?}");

    // The backend filter (zero-message only) hides NOTHING here — every row
    // carries messages, so everything survives to the view partition.
    let backend = filter_web_noise_rows(coalesce_web_import_rows(rows));
    assert_eq!(
        backend.len(),
        10,
        "no seed is zero-message; the backend must keep all ten: {backend:?}"
    );

    let (regular, temporary) = partition_web_noise_rows(backend);
    let temp_ids: Vec<&str> = temporary.iter().map(|row| row.session_id.as_str()).collect();
    let regular_ids: Vec<&str> = regular.iter().map(|row| row.session_id.as_str()).collect();

    // The temporary rule covers exactly the unnamed, tiny, native rows whose
    // cwd sits under the OS temp root — the temp-worktree workflow shapes and
    // the legal-but-temp user experiment (documented boundary). It never
    // covers legal rows in real workspaces.
    assert!(
        temp_ids.contains(&"fx-workflow-supervisor"),
        "fx-workflow-supervisor must be temporary: {temp_ids:?}"
    );
    assert!(
        temp_ids.contains(&"fx-workflow-child"),
        "fx-workflow-child must be temporary: {temp_ids:?}"
    );
    assert!(
        temp_ids.contains(&"user-tmp-experiment"),
        "the legal-but-temp boundary row must be temporary (recoverable): {temp_ids:?}"
    );
    for id in [
        "fx-clone-row",
        "fx-rewind-row",
        "user-short",
        "user-image",
        "user-named-ship",
        "user-fork",
        "user-custom-provider",
    ] {
        assert!(
            regular_ids.contains(&id),
            "{id} must stay regular (never temporary): regular={regular_ids:?} temp={temp_ids:?}"
        );
    }
    assert_eq!(regular_ids.len() + temp_ids.len(), 10);

    let conclusion = format!(
        "The temporary rule (native + unnamed + <10KiB + cwd under the OS temp root) \
         covers the temp-workspace harness shapes ({temp_covered} rows: {temp_covered_ids}) \
         as a RECOVERABLE view signal — never a backend filter or deletion (search query, \
         loaded, and active rows stay visible). Zero-message <10KiB rows are the only rows \
         the backend still hides. {regular_count} rows ({regular_ids}) carry real messages in \
         real-looking cwds or names and are indistinguishable from legal sessions: they must \
         be cleaned up by the USER with explicit consent (e.g. a Web panel listing candidates), \
         never auto-deleted by a heuristic.",
        temp_covered = temp_ids.len(),
        temp_covered_ids = temp_ids.join(", "),
        regular_count = regular_ids.len(),
        regular_ids = regular_ids.join(", "),
    );
    let evidence = json!({
        "dataset": {
            "fixtureRows": fixture_ids,
            "legalUserRows": legal_ids,
        },
        "temporaryRows": temp_ids,
        "regularRows": regular_ids,
        "backendFilter": {
            "predicate": "size < 10KiB AND messageCount == 0 (backend, irreversible)",
            "rowsHidden": [],
        },
        "temporaryRule": {
            "predicate": "native && unnamed && <10KiB && cwd under OS temp root (view-only, recoverable)",
            "rowsCovered": temp_ids,
        },
        "needUserCleanup": ["fx-clone-row", "fx-rewind-row"],
        "conclusion": conclusion,
        "recommendation": "user-level cleanup/migration with explicit consent for rows outside the temporary rule; temporary rows stay recoverable via search / active / loaded",
    });

    let evidence_dir = std::env::var_os("CARGO_TARGET_TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let evidence_path = evidence_dir.join("historical-fixture-filter-eval.json");
    fs_write(&evidence_path, serde_json::to_string_pretty(&evidence).expect("serialize evidence"));
    eprintln!("historical-fixture-filter-eval evidence: {}", evidence_path.display());
}
