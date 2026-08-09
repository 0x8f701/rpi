//! Host-level hooks: external command hooks fired at host events.
//!
//! Modeled on Grok `$GROK_HOME/hooks/` and Claude Code settings hooks. Hook
//! entries are plain external commands (argv, no shell) declared in
//! `Settings.hooks`. On every matching event the hook receives a JSON payload
//! on stdin — event, subject, tool name/args summary, cwd, session id,
//! timestamp; **no secrets** — and its capped stdout is parsed as JSON.
//!
//! Only `pre_tool_call` and `pre_trust_decision` can block: a
//! `{"decision":"block","reason":"..."}` response prevents the tool from
//! running or denies the tentative trust decision. Every other event is
//! advisory (logged). Hook failures (spawn error, non-zero exit, timeout,
//! malformed JSON) fail *open* for those two events unless the entry sets
//! `fail_closed: true`, in which case the tool is blocked (or the trust
//! decision denied) instead.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::time::timeout;

use crate::settings::{HookConfig, HookEvent};

pub const HOOK_DEFAULT_TIMEOUT_MS: u64 = 5_000;
pub const HOOK_MAX_TIMEOUT_MS: u64 = 60_000;
/// Hard cap on hook stdout; bytes beyond this are drained and discarded.
pub const HOOK_OUTPUT_CAP_BYTES: usize = 64 * 1024;
const HOOK_ARGUMENTS_SUMMARY_CAP_BYTES: usize = 4 * 1024;
const HOOK_RESULT_SUMMARY_CAP_BYTES: usize = 4 * 1024;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HookDecision {
    pub block: bool,
    pub reason: Option<String>,
}

impl HookDecision {
    #[must_use]
    pub fn allow() -> Self {
        Self { block: false, reason: None }
    }

    #[must_use]
    pub fn block(reason: impl Into<String>) -> Self {
        Self { block: true, reason: Some(reason.into()) }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct HookToolPayload<'a> {
    pub name: &'a str,
    pub arguments: Option<&'a Value>,
    /// Text summary of the tool result (post_tool_call only).
    pub result_text: Option<&'a str>,
    pub is_error: bool,
}

/// Events whose `{"decision":"block"}` responses are honored by the executor
/// (and whose failures honor `fail_closed`). Every other event is advisory.
const fn is_blocking_event(event: HookEvent) -> bool {
    matches!(event, HookEvent::PreToolCall | HookEvent::PreTrustDecision)
}

#[derive(Clone)]
pub struct HostHooks {
    inner: Arc<HostHooksInner>,
}

struct HostHooksInner {
    entries: Vec<HookConfig>,
    cwd: PathBuf,
    session_id: String,
}

impl std::fmt::Debug for HostHooks {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HostHooks")
            .field("entries", &self.inner.entries.len())
            .field("cwd", &self.inner.cwd)
            .field("session_id", &self.inner.session_id)
            .finish()
    }
}

impl HostHooks {
    #[must_use]
    pub fn new(entries: Vec<HookConfig>, cwd: PathBuf, session_id: impl Into<String>) -> Self {
        Self {
            inner: Arc::new(HostHooksInner {
                entries,
                cwd,
                session_id: session_id.into(),
            }),
        }
    }

    /// No enabled entry for any event.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner
            .entries
            .iter()
            .all(|entry| entry.enabled == Some(false))
    }

    /// Number of configured entries (used by diagnostics).
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.entries.len()
    }

    /// Fire every enabled hook registered for `event`, in declaration order.
    ///
    /// `subject` is matched against each entry's `matcher` (exact or
    /// substring). A block decision short-circuits the remaining entries.
    pub async fn fire(
        &self,
        event: HookEvent,
        subject: Option<&str>,
        tool: Option<&HookToolPayload<'_>>,
    ) -> HookDecision {
        let payload = build_payload(&self.inner, event, subject, tool);
        self.fire_payload(event, subject, payload).await
    }

    /// Fire `pre_trust_decision` hooks for a tentative trust decision, before
    /// the stored decision is consulted/recorded.
    ///
    /// The payload carries the canonical project path, the tentative decision
    /// (`trusted`/`untrusted`/`ask`), and whether the path is new to the trust
    /// store. A `{"decision":"block"}` response denies the trust decision
    /// (the host applies it via [`crate::trust::apply_trust_hook_outcomes`]);
    /// failures fail open unless the entry sets `fail_closed: true`.
    pub async fn fire_trust_decision(
        &self,
        path: &str,
        decision: &str,
        is_new: bool,
    ) -> HookDecision {
        let payload = build_trust_payload(&self.inner, path, decision, is_new);
        self.fire_payload(HookEvent::PreTrustDecision, Some(path), payload)
            .await
    }

    async fn fire_payload(
        &self,
        event: HookEvent,
        subject: Option<&str>,
        payload: Value,
    ) -> HookDecision {
        for entry in &self.inner.entries {
            if entry.enabled == Some(false) || entry.event != event {
                continue;
            }
            if let Some(matcher) = entry.matcher.as_deref()
                && let Some(subject) = subject
                && subject != matcher
                && !subject.contains(matcher)
            {
                continue;
            }
            let decision = self.run_one(entry, &payload).await;
            if decision.block {
                return decision;
            }
        }
        HookDecision::allow()
    }

    async fn run_one(&self, entry: &HookConfig, payload: &Value) -> HookDecision {
        let event_name = entry.event.as_str();
        let Some(program) = entry.command.first().cloned() else {
            eprintln!("hooks: skipping {event_name} entry with empty command");
            return HookDecision::allow();
        };
        let timeout_ms = entry
            .timeout_ms
            .unwrap_or(HOOK_DEFAULT_TIMEOUT_MS)
            .min(HOOK_MAX_TIMEOUT_MS);
        let mut command = Command::new(&program);
        command.args(&entry.command[1..]);
        command.current_dir(&self.inner.cwd);
        command.stdin(Stdio::piped());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::null());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.as_std_mut().process_group(0);
        }
        let mut child = match spawn_with_etxtbsy_retry(&mut command, &program, event_name).await {
            Ok(child) => child,
            Err(error) => {
                eprintln!("hooks: failed to spawn {program:?} for {event_name}: {error}");
                return self.failure_decision(entry, &program);
            }
        };
        let pid = child.id();
        let payload_json = serde_json::to_string(payload).unwrap_or_else(|_| "{}".to_owned());
        // Everything (stdin write, stdout read, wait) runs inside the timeout so
        // a hook that never reads stdin and runs forever still gets killed.
        let outcome = timeout(Duration::from_millis(timeout_ms), async {
            if let Some(mut stdin) = child.stdin.take() {
                let write = stdin.write_all(payload_json.as_bytes()).await;
                // Drop stdin so the hook observes EOF once it has consumed the
                // payload (or a blocking hook never returns).
                drop(stdin);
                if let Err(error) = write
                    && error.kind() != std::io::ErrorKind::BrokenPipe
                {
                    return Err(error);
                }
                // Broken pipe means the hook exited without reading stdin; its
                // exit status and stdout are still meaningful, so continue.
            }
            let stdout = child.stdout.take();
            let output = match stdout {
                Some(stdout) => read_capped(stdout, HOOK_OUTPUT_CAP_BYTES).await,
                None => Vec::new(),
            };
            let status = child.wait().await?;
            Ok::<_, std::io::Error>((status, output))
        })
        .await;
        match outcome {
            Ok(Ok((status, output))) => {
                if !status.success() {
                    eprintln!("hooks: {program:?} for {event_name} exited with {status}");
                    return self.failure_decision(entry, &program);
                }
                self.parse_response(entry, &program, &output)
            }
            Ok(Err(error)) => {
                eprintln!("hooks: {program:?} for {event_name} failed: {error}");
                kill_process_group(pid);
                let _ = child.wait().await;
                self.failure_decision(entry, &program)
            }
            Err(_elapsed) => {
                eprintln!("hooks: {program:?} for {event_name} timed out after {timeout_ms}ms; killed");
                kill_process_group(pid);
                let _ = child.wait().await;
                self.failure_decision(entry, &program)
            }
        }
    }

    fn parse_response(&self, entry: &HookConfig, program: &str, output: &[u8]) -> HookDecision {
        if !is_blocking_event(entry.event) {
            // Advisory events never block; their stdout is informational only.
            return HookDecision::allow();
        }
        let text = String::from_utf8_lossy(output);
        let response: HookResponse = match serde_json::from_str(text.trim()) {
            Ok(response) => response,
            Err(error) => {
                eprintln!(
                    "hooks: {program:?} for {} returned non-JSON stdout: {error}",
                    entry.event.as_str()
                );
                return self.failure_decision(entry, program);
            }
        };
        match response.decision.as_deref() {
            Some("block") => HookDecision::block(
                response
                    .reason
                    .unwrap_or_else(|| "blocked by host hook".to_owned()),
            ),
            Some("allow") | None => HookDecision::allow(),
            Some(decision) => {
                eprintln!(
                    "hooks: {program:?} for {} returned unknown decision {decision:?}",
                    entry.event.as_str()
                );
                self.failure_decision(entry, program)
            }
        }
    }

    fn failure_decision(&self, entry: &HookConfig, program: &str) -> HookDecision {
        if is_blocking_event(entry.event) && entry.fail_closed == Some(true) {
            HookDecision::block(format!(
                "host hook {program:?} failed for {} (failClosed)",
                entry.event.as_str()
            ))
        } else {
            HookDecision::allow()
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
struct HookResponse {
    #[serde(default)]
    decision: Option<String>,
    #[serde(default)]
    reason: Option<String>,
}

fn build_payload(
    inner: &HostHooksInner,
    event: HookEvent,
    subject: Option<&str>,
    tool: Option<&HookToolPayload<'_>>,
) -> Value {
    let mut payload = build_envelope(inner, event);
    if let Some(subject) = subject {
        payload.insert("subject".to_owned(), Value::String(subject.to_owned()));
    }
    if let Some(tool) = tool {
        payload.insert("toolName".to_owned(), Value::String(tool.name.to_owned()));
        if let Some(arguments) = tool.arguments {
            let summary = truncate_json(arguments, HOOK_ARGUMENTS_SUMMARY_CAP_BYTES);
            payload.insert("arguments".to_owned(), Value::String(summary));
        }
        if let Some(result_text) = tool.result_text {
            let summary = truncate_text(result_text, HOOK_RESULT_SUMMARY_CAP_BYTES);
            payload.insert("result".to_owned(), Value::String(summary));
        }
        payload.insert("isError".to_owned(), Value::Bool(tool.is_error));
    }
    Value::Object(payload)
}

/// Payload for `pre_trust_decision`: the standard envelope plus the canonical
/// project path, the tentative decision (`trusted`/`untrusted`/`ask`), and
/// whether the path is new to the trust store. The `path`/`decision`/`isNew`
/// triple matches the `trust_decision` extension event payload so both
/// surfaces observe one spelling (see [`crate::trust::TrustDecisionObservation`]).
fn build_trust_payload(
    inner: &HostHooksInner,
    path: &str,
    decision: &str,
    is_new: bool,
) -> Value {
    let mut payload = build_envelope(inner, HookEvent::PreTrustDecision);
    payload.insert("subject".to_owned(), Value::String(path.to_owned()));
    payload.insert("path".to_owned(), Value::String(path.to_owned()));
    payload.insert("decision".to_owned(), Value::String(decision.to_owned()));
    payload.insert("isNew".to_owned(), Value::Bool(is_new));
    Value::Object(payload)
}

/// The fields every hook payload shares: event name, cwd, session id, and a
/// millisecond timestamp. **No secrets.**
fn build_envelope(inner: &HostHooksInner, event: HookEvent) -> Map<String, Value> {
    let mut payload = Map::new();
    payload.insert("event".to_owned(), Value::String(event.as_str().to_owned()));
    payload.insert(
        "cwd".to_owned(),
        Value::String(inner.cwd.to_string_lossy().into_owned()),
    );
    payload.insert("sessionId".to_owned(), Value::String(inner.session_id.clone()));
    payload.insert(
        "timestamp".to_owned(),
        Value::Number(serde_json::Number::from(pi_ai::now_millis())),
    );
    payload
}

/// Serialize a JSON value as a truncated string summary (never leaks secrets
/// beyond the arguments the host already passes to the tool).
fn truncate_json(value: &Value, cap: usize) -> String {
    let serialized = serde_json::to_string(value).unwrap_or_else(|_| "{}".to_owned());
    truncate_text(&serialized, cap)
}

fn truncate_text(text: &str, cap: usize) -> String {
    if text.len() <= cap {
        text.to_owned()
    } else {
        let mut truncated = text.chars().take(cap.saturating_sub(3)).collect::<String>();
        truncated.push_str("...");
        truncated
    }
}

/// Read at most `cap` bytes while draining the remainder so a chatty hook can
/// never deadlock its parent on a full pipe.
async fn read_capped(mut reader: impl AsyncRead + Unpin, cap: usize) -> Vec<u8> {
    let mut output = Vec::with_capacity(cap.min(8192));
    let mut buffer = [0u8; 8192];
    loop {
        let Ok(read) = reader.read(&mut buffer).await else {
            break;
        };
        if read == 0 {
            break;
        }
        if output.len() < cap {
            let take = (cap - output.len()).min(read);
            output.extend_from_slice(&buffer[..take]);
        }
    }
    output
}

/// Spawn a hook command, retrying briefly on `ETXTBSY`.
///
/// A hook executable that was just written to disk can transiently hit
/// "Text file busy" while the kernel settles the file's open-for-write state
/// (observed on overlay/tmp filesystems under parallel test load). The
/// executable itself is already closed by then, so a short retry resolves it.
pub(crate) async fn spawn_with_etxtbsy_retry(
    command: &mut Command,
    program: &str,
    event_name: &str,
) -> std::io::Result<tokio::process::Child> {
    let mut attempts = 0u32;
    loop {
        match command.spawn() {
            Ok(child) => return Ok(child),
            Err(error) if retryable_etxtbsy(&error) => {
                attempts += 1;
                if attempts >= 5 {
                    return Err(error);
                }
                eprintln!(
                    "hooks: {program:?} for {event_name} hit ETXTBSY (attempt {attempts}); retrying"
                );
                tokio::time::sleep(Duration::from_millis(10 * u64::from(attempts))).await;
            }
            Err(error) => return Err(error),
        }
    }
}

/// Detect `ETXTBSY` (Text file busy, os error 26) from a spawn error. Uses the
/// raw OS error because `ErrorKind` is `Uncategorized` for unclassified errno
/// values.
pub(crate) fn retryable_etxtbsy(error: &std::io::Error) -> bool {
    #[cfg(unix)]
    {
        error.raw_os_error() == Some(nix::errno::Errno::ETXTBSY as i32)
    }
    #[cfg(not(unix))]
    {
        let _ = error;
        false
    }
}

fn kill_process_group(pid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = pid {
        use nix::sys::signal::{Signal, killpg};
        use nix::unistd::Pid;
        let _ = killpg(Pid::from_raw(pid as i32), Signal::SIGKILL);
    }
    #[cfg(not(unix))]
    let _ = pid;
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use uuid::Uuid;

    use super::*;

    fn hook_config(
        event: HookEvent,
        command: &[&str],
        matcher: Option<&str>,
        fail_closed: bool,
    ) -> HookConfig {
        HookConfig {
            event,
            matcher: matcher.map(str::to_owned),
            command: command.iter().map(|part| part.to_string()).collect(),
            timeout_ms: Some(2_000),
            enabled: None,
            fail_closed: Some(fail_closed),
            extra: Map::new(),
        }
    }

    fn hooks(entries: Vec<HookConfig>) -> HostHooks {
        HostHooks::new(entries, std::env::current_dir().expect("cwd"), "hook-test-session")
    }

    /// Write an executable fixture hook into a temp dir (returned so the
    /// script stays on disk for the duration of the test). The script is
    /// written under a unique temp name and atomically renamed so the exec'd
    /// path was never itself open for writing (avoids transient ETXTBSY).
    fn write_hook(body: &str) -> (String, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("hook dir");
        let tmp = dir.path().join(format!("hook.tmp-{}", Uuid::now_v7()));
        std::fs::write(&tmp, body).expect("write hook script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&tmp).expect("metadata").permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&tmp, permissions).expect("chmod hook");
        }
        let path = dir.path().join("hook.sh");
        std::fs::rename(&tmp, &path).expect("rename hook into place");
        (path.to_string_lossy().into_owned(), dir)
    }

    #[tokio::test]
    async fn payload_round_trip_is_json_on_stdin_and_stdout_is_parsed() {
        let (script_path, _dir) = write_hook(
            r#"#!/bin/sh
read -r line
event=$(printf '%s' "$line" | sed -n 's/.*"event":"\([^"]*\)".*/\1/p')
echo "{\"decision\":\"block\",\"reason\":\"stdin said $event\"}"
"#,
        );
        let hooks = hooks(vec![hook_config(
            HookEvent::PreToolCall,
            &[&script_path],
            None,
            false,
        )]);
        let decision = hooks
            .fire(
                HookEvent::PreToolCall,
                Some("read"),
                Some(&HookToolPayload {
                    name: "read",
                    arguments: Some(&json!({ "path": "a.txt" })),
                    result_text: None,
                    is_error: false,
                }),
            )
            .await;
        assert!(decision.block);
        let reason = decision.reason.expect("block reason");
        assert!(reason.contains("pre_tool_call"), "reason: {reason}");
    }

    #[tokio::test]
    async fn timeout_kills_sleeping_hook_and_fails_open() {
        let (script_path, _dir) = write_hook("#!/bin/sh\nsleep 30\n");
        let mut entry = hook_config(HookEvent::PreToolCall, &[&script_path], None, false);
        entry.timeout_ms = Some(100);
        let hooks = hooks(vec![entry]);
        let decision = hooks
            .fire(HookEvent::PreToolCall, Some("read"), None)
            .await;
        assert!(!decision.block, "timeout must fail open by default");
    }

    #[tokio::test]
    async fn timeout_fails_closed_when_configured() {
        let (script_path, _dir) = write_hook("#!/bin/sh\nsleep 30\n");
        let mut entry = hook_config(HookEvent::PreToolCall, &[&script_path], None, true);
        entry.timeout_ms = Some(100);
        let hooks = hooks(vec![entry]);
        let decision = hooks
            .fire(HookEvent::PreToolCall, Some("read"), None)
            .await;
        assert!(decision.block, "failClosed must block on timeout");
        assert!(
            decision.reason.as_deref().is_some_and(|reason| reason.contains("failClosed")),
            "reason: {:?}",
            decision.reason
        );
    }

    #[tokio::test]
    async fn spawn_failure_fails_open_unless_fail_closed() {
        let missing = "/definitely/not/a/real/hook/binary";
        let open = hooks(vec![hook_config(
            HookEvent::PreToolCall,
            &[missing],
            None,
            false,
        )]);
        let decision = open.fire(HookEvent::PreToolCall, Some("read"), None).await;
        assert!(!decision.block, "spawn failure must fail open by default");

        let closed = hooks(vec![hook_config(
            HookEvent::PreToolCall,
            &[missing],
            None,
            true,
        )]);
        let decision = closed.fire(HookEvent::PreToolCall, Some("read"), None).await;
        assert!(decision.block, "failClosed must block on spawn failure");
    }

    #[tokio::test]
    async fn nonzero_exit_fails_open_by_default() {
        let (script_path, _dir) = write_hook("#!/bin/sh\necho '{\"decision\":\"block\"}'\nexit 3\n");
        let hooks = hooks(vec![hook_config(
            HookEvent::PreToolCall,
            &[&script_path],
            None,
            false,
        )]);
        let decision = hooks.fire(HookEvent::PreToolCall, Some("read"), None).await;
        assert!(!decision.block, "non-zero exit must fail open");
    }

    #[tokio::test]
    async fn matcher_filters_entries_by_subject() {
        let (script_path, _dir) = write_hook("#!/bin/sh\necho '{\"decision\":\"block\",\"reason\":\"matched\"}'\n");
        // Substring match on the subject: blocks `read`-family tools only.
        let hooks = hooks(vec![hook_config(
            HookEvent::PreToolCall,
            &[&script_path],
            Some("read"),
            false,
        )]);
        let decision = hooks
            .fire(HookEvent::PreToolCall, Some("bash"), None)
            .await;
        assert!(!decision.block, "bash must not match matcher \"read\"");
        let decision = hooks
            .fire(HookEvent::PreToolCall, Some("read"), None)
            .await;
        assert!(decision.block, "read must match matcher \"read\"");
    }

    #[tokio::test]
    async fn disabled_entries_are_skipped() {
        let (script_path, _dir) = write_hook("#!/bin/sh\nexit 7\n");
        // Exit 7 would fail closed if the entry ran; `enabled: false` must skip it.
        let mut entry = hook_config(HookEvent::PreToolCall, &[&script_path], None, true);
        entry.enabled = Some(false);
        let hooks = hooks(vec![entry]);
        let decision = hooks
            .fire(HookEvent::PreToolCall, Some("read"), None)
            .await;
        assert!(!decision.block, "disabled entry must not fire");
    }

    #[tokio::test]
    async fn post_tool_call_is_advisory_even_when_blocking() {
        let (script_path, _dir) = write_hook("#!/bin/sh\necho '{\"decision\":\"block\",\"reason\":\"ignored\"}'\n");
        let hooks = hooks(vec![hook_config(
            HookEvent::PostToolCall,
            &[&script_path],
            None,
            false,
        )]);
        let decision = hooks
            .fire(
                HookEvent::PostToolCall,
                Some("read"),
                Some(&HookToolPayload {
                    name: "read",
                    arguments: None,
                    result_text: Some("file contents"),
                    is_error: false,
                }),
            )
            .await;
        assert!(!decision.block, "advisory events never block");
    }

    #[tokio::test]
    async fn stdout_cap_drains_overflow_without_hanging() {
        let (script_path, _dir) = write_hook("#!/bin/sh\nhead -c 200000 /dev/zero | tr '\\0' 'x'\n");
        let hooks = hooks(vec![hook_config(
            HookEvent::PreToolCall,
            &[&script_path],
            None,
            false,
        )]);
        let decision = hooks.fire(HookEvent::PreToolCall, Some("read"), None).await;
        assert!(!decision.block, "invalid JSON after cap must fail open");
    }

    #[tokio::test]
    async fn hook_reading_stdin_receives_full_payload() {
        let (script_path, _dir) = write_hook(
            "#!/bin/sh\nread -r payload\nprintf '%s' \"$payload\" > \"$1\"\necho '{\"decision\":\"allow\"}'\n",
        );
        let out_dir = tempfile::tempdir().expect("out dir");
        let out_file = out_dir.path().join("payload.json");
        let out_file_text = out_file.to_string_lossy().into_owned();
        let hooks = hooks(vec![HookConfig {
            event: HookEvent::PreToolCall,
            matcher: None,
            command: vec![script_path, out_file_text],
            timeout_ms: Some(2_000),
            enabled: None,
            fail_closed: None,
            extra: Map::new(),
        }]);
        let decision = hooks
            .fire(
                HookEvent::PreToolCall,
                Some("read"),
                Some(&HookToolPayload {
                    name: "read",
                    arguments: Some(&json!({ "path": "a.txt" })),
                    result_text: None,
                    is_error: false,
                }),
            )
            .await;
        assert!(!decision.block);
        let captured = std::fs::read_to_string(&out_file).expect("captured payload");
        let parsed: Value = serde_json::from_str(&captured).expect("payload is JSON");
        assert_eq!(parsed["event"], "pre_tool_call");
        assert_eq!(parsed["subject"], "read");
        assert_eq!(parsed["toolName"], "read");
        assert_eq!(parsed["sessionId"], "hook-test-session");
        assert!(parsed["arguments"].as_str().unwrap().contains("\"path\":\"a.txt\""));
        assert!(parsed["timestamp"].as_i64().is_some());
    }

    #[tokio::test]
    async fn write_payload_survives_pipe_backpressure() {
        // A hook that reads stdin slowly must still receive the payload and
        // EOF before the timeout.
        let (script_path, _dir) = write_hook("#!/bin/sh\nwhile IFS= read -r chunk; do :; done\necho '{\"decision\":\"allow\"}'\n");
        let hooks = hooks(vec![hook_config(
            HookEvent::PreToolCall,
            &[&script_path],
            None,
            false,
        )]);
        let decision = hooks.fire(HookEvent::PreToolCall, Some("read"), None).await;
        assert!(!decision.block);
    }

    #[tokio::test]
    async fn event_serialization_uses_snake_case() {
        assert_eq!(serde_json::to_string(&HookEvent::PreToolCall).unwrap(), "\"pre_tool_call\"");
        assert_eq!(serde_json::to_string(&HookEvent::SessionEnd).unwrap(), "\"session_end\"");
        assert_eq!(HookEvent::TurnStart.as_str(), "turn_start");
    }

    #[tokio::test]
    async fn payload_has_no_env_secrets() {
        // The payload must only contain the documented fields; the full env is
        // not serialized into it.
        let inner = HostHooksInner {
            entries: Vec::new(),
            cwd: PathBuf::from("/tmp"),
            session_id: "s".to_owned(),
        };
        let payload = build_payload(&inner, HookEvent::SessionStart, None, None);
        let object = payload.as_object().expect("object");
        assert_eq!(object.len(), 4);
        assert!(object.contains_key("event"));
        assert!(object.contains_key("cwd"));
        assert!(object.contains_key("sessionId"));
        assert!(object.contains_key("timestamp"));
    }

    #[test]
    fn truncate_text_caps_and_marks() {
        assert_eq!(truncate_text("short", 100), "short");
        let truncated = truncate_text("x".repeat(200).as_str(), 100);
        assert!(truncated.ends_with("..."));
        assert!(truncated.len() <= 100);
    }

    #[tokio::test]
    async fn pre_trust_decision_hook_blocks_and_denies() {
        let (script_path, _dir) =
            write_hook("#!/bin/sh\necho '{\"decision\":\"block\",\"reason\":\"deny project\"}'\n");
        let hooks = hooks(vec![hook_config(
            HookEvent::PreTrustDecision,
            &[&script_path],
            None,
            false,
        )]);
        let decision = hooks
            .fire_trust_decision("/tmp/project", "trusted", true)
            .await;
        assert!(decision.block);
        let reason = decision.reason.expect("block reason");
        assert!(reason.contains("deny project"), "reason: {reason}");
    }

    #[tokio::test]
    async fn pre_trust_decision_timeout_fails_open_unless_fail_closed() {
        let (script_path, _dir) = write_hook("#!/bin/sh\nsleep 30\n");
        let mut open = hook_config(HookEvent::PreTrustDecision, &[&script_path], None, false);
        open.timeout_ms = Some(100);
        let open_hooks = hooks(vec![open]);
        let decision = open_hooks
            .fire_trust_decision("/tmp/project", "ask", true)
            .await;
        assert!(!decision.block, "timeout must fail open by default");

        let (script_path, _dir) = write_hook("#!/bin/sh\nsleep 30\n");
        let mut closed = hook_config(HookEvent::PreTrustDecision, &[&script_path], None, true);
        closed.timeout_ms = Some(100);
        let closed_hooks = hooks(vec![closed]);
        let decision = closed_hooks
            .fire_trust_decision("/tmp/project", "ask", true)
            .await;
        assert!(decision.block, "failClosed must deny on timeout");
        assert!(
            decision
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("failClosed")),
            "reason: {:?}",
            decision.reason
        );
    }

    #[tokio::test]
    async fn pre_trust_decision_payload_carries_path_decision_and_is_new() {
        let (script_path, _dir) = write_hook(
            "#!/bin/sh\nread -r payload\nprintf '%s' \"$payload\" > \"$1\"\necho '{\"decision\":\"allow\"}'\n",
        );
        let out_dir = tempfile::tempdir().expect("out dir");
        let out_file = out_dir.path().join("payload.json");
        let out_file_text = out_file.to_string_lossy().into_owned();
        let hooks = hooks(vec![HookConfig {
            event: HookEvent::PreTrustDecision,
            matcher: None,
            command: vec![script_path, out_file_text],
            timeout_ms: Some(2_000),
            enabled: None,
            fail_closed: None,
            extra: Map::new(),
        }]);
        let decision = hooks
            .fire_trust_decision("/tmp/project", "untrusted", false)
            .await;
        assert!(!decision.block);
        let captured = std::fs::read_to_string(&out_file).expect("captured payload");
        let parsed: Value = serde_json::from_str(&captured).expect("payload is JSON");
        assert_eq!(parsed["event"], "pre_trust_decision");
        assert_eq!(parsed["subject"], "/tmp/project");
        assert_eq!(parsed["path"], "/tmp/project");
        assert_eq!(parsed["decision"], "untrusted");
        assert_eq!(parsed["isNew"], false);
        assert_eq!(parsed["sessionId"], "hook-test-session");
        assert!(parsed["timestamp"].as_i64().is_some());
    }

    #[tokio::test]
    async fn pre_trust_decision_garbage_stdout_fails_open() {
        let (script_path, _dir) = write_hook("#!/bin/sh\necho 'not json'\n");
        let hooks = hooks(vec![hook_config(
            HookEvent::PreTrustDecision,
            &[&script_path],
            None,
            false,
        )]);
        let decision = hooks
            .fire_trust_decision("/tmp/project", "trusted", false)
            .await;
        assert!(!decision.block, "malformed stdout must fail open");
    }

    #[tokio::test]
    async fn pre_trust_decision_matcher_filters_by_path() {
        let (script_path, _dir) = write_hook("#!/bin/sh\necho '{\"decision\":\"block\"}'\n");
        let unmatched = hooks(vec![hook_config(
            HookEvent::PreTrustDecision,
            &[&script_path],
            Some("other"),
            false,
        )]);
        let decision = unmatched
            .fire_trust_decision("/tmp/project", "trusted", true)
            .await;
        assert!(!decision.block, "path must not match matcher \"other\"");

        let matched = hooks(vec![hook_config(
            HookEvent::PreTrustDecision,
            &[&script_path],
            Some("project"),
            false,
        )]);
        let decision = matched
            .fire_trust_decision("/tmp/project", "trusted", true)
            .await;
        assert!(decision.block, "path must match matcher \"project\"");
    }

    #[tokio::test]
    async fn pre_trust_decision_spawn_failure_fails_open_unless_fail_closed() {
        let missing = "/definitely/not/a/real/hook/binary";
        let open = hooks(vec![hook_config(
            HookEvent::PreTrustDecision,
            &[missing],
            None,
            false,
        )]);
        let decision = open
            .fire_trust_decision("/tmp/project", "trusted", true)
            .await;
        assert!(!decision.block, "spawn failure must fail open by default");

        let closed = hooks(vec![hook_config(
            HookEvent::PreTrustDecision,
            &[missing],
            None,
            true,
        )]);
        let decision = closed
            .fire_trust_decision("/tmp/project", "trusted", true)
            .await;
        assert!(decision.block, "failClosed must deny on spawn failure");
    }

    #[tokio::test]
    async fn pre_trust_decision_event_serialization_uses_snake_case() {
        assert_eq!(
            serde_json::to_string(&HookEvent::PreTrustDecision).unwrap(),
            "\"pre_trust_decision\""
        );
        assert_eq!(HookEvent::PreTrustDecision.as_str(), "pre_trust_decision");
    }
}
