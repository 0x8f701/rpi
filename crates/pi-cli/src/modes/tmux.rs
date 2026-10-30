use std::cell::Cell;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::{Arc, LazyLock};
#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use base64::Engine as _;
use parking_lot::Mutex;
use pi_coding::redact::{redact_bounded, redact_secrets};
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde::Serialize;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

const TMUX_TIMEOUT: Duration = Duration::from_secs(2);
const TMUX_LIST_STDOUT_CAP: usize = 64 * 1024;
const TMUX_CAPTURE_STDOUT_CAP: usize = 128 * 1024;
const TMUX_TERMINAL_CAPTURE_START: &str = "-10000";
const TMUX_TERMINAL_CAPTURE_STDOUT_CAP: usize = 2 * 1024 * 1024;
const TMUX_STDERR_CAP: usize = 8 * 1024;
const TMUX_CONTENT_CHARS: usize = 32_000;
const TMUX_SESSION_NAME_MAX: usize = 128;
const TMUX_SEND_TEXT_MAX: usize = 4096;
const TMUX_TERMINAL_MAX: usize = 16;
const TMUX_TERMINAL_COLS_MIN: u32 = 20;
const TMUX_TERMINAL_COLS_MAX: u32 = 400;
const TMUX_TERMINAL_ROWS_MIN: u32 = 5;
const TMUX_TERMINAL_ROWS_MAX: u32 = 120;
const TMUX_TERMINAL_INPUT_MAX: usize = 64 * 1024;
const TMUX_TERMINAL_OUTPUT_RAW_MAX: usize = 12 * 1024;
const TMUX_TERMINAL_FLUSH: Duration = Duration::from_millis(50);
const TMUX_TERMINAL_TERM: &str = "xterm-256color";
const TMUX_TERMINAL_PTY_CHUNK: usize = 8192;
const TMUX_TERMINAL_RAW_QUEUE: usize = 32;
const TMUX_TERMINAL_EVENT_QUEUE: usize = 32;
const TMUX_TERMINAL_WRITE_QUEUE: usize = 8;
const TMUX_TERMINAL_WRITE_SEND: Duration = Duration::from_millis(50);
const TMUX_TERMINAL_CLOSED_OWNERS_MAX: usize = 4096;
const LIST_SESSIONS_FORMAT: &str =
    "#{session_name}\t#{session_windows}\t#{session_attached}\t#{session_created}";
const PANE_LIST_FORMAT: &str = "#{session_name}";
const PANE_META_FORMAT: &str = "#{pane_id}\t#{pane_width}\t#{pane_height}\t#{start_time}";

struct PaneHistoryEntry {
    session: String,
    bytes: Arc<Vec<u8>>,
}

static PANE_HISTORY: LazyLock<Mutex<HashMap<String, PaneHistoryEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn pane_history_key(daemon_epoch: &str, session: &str, pane_key: &str) -> String {
    format!("{daemon_epoch}:{session}:{pane_key}")
}

fn pane_history_lookup(key: &str) -> Option<Arc<Vec<u8>>> {
    PANE_HISTORY.lock().get(key).map(|entry| Arc::clone(&entry.bytes))
}

fn pane_history_store(key: String, session: String, bytes: Arc<Vec<u8>>) {
    PANE_HISTORY.lock().insert(key, PaneHistoryEntry { session, bytes });
}

fn purge_pane_history_for_session(session: &str) {
    PANE_HISTORY.lock().retain(|_, entry| entry.session != session);
}

#[cfg(test)]
static FAIL_SETUP_OWNER: Mutex<Option<Uuid>> = Mutex::new(None);
#[cfg(test)]
static HOLD_RECV_OWNER: Mutex<Option<Uuid>> = Mutex::new(None);
#[cfg(test)]
static LAST_SPAWNED_CHILD: Mutex<Vec<(Uuid, u32)>> = Mutex::new(Vec::new());
#[cfg(test)]
static ATTACH_BEFORE_RECV_READY: AtomicBool = AtomicBool::new(false);
#[cfg(test)]
static ATTACH_HOLD_RELEASE: AtomicBool = AtomicBool::new(false);
#[cfg(test)]
static STALL_WRITER_OWNERS: Mutex<Vec<Uuid>> = Mutex::new(Vec::new());
#[cfg(test)]
static HOLD_AFTER_INSERT_OWNER: Mutex<Option<Uuid>> = Mutex::new(None);
#[cfg(test)]
static ATTACH_AFTER_INSERT_READY: AtomicBool = AtomicBool::new(false);
#[cfg(test)]
static ATTACH_AFTER_INSERT_RELEASE: AtomicBool = AtomicBool::new(false);

pub(crate) const TMUX_TERMINAL_WRITE_TIMED_OUT: &str = "tmux terminal write timed out";
static SESSION_TERMINAL_COUNTS: Mutex<BTreeMap<String, usize>> = Mutex::new(BTreeMap::new());

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TmuxSessionList {
    pub tmux_available: bool,
    pub sessions: Vec<TmuxSession>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TmuxSession {
    pub name: String,
    pub windows: u32,
    pub attached: bool,
    pub created_epoch: i64,
    pub pane_count: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct TmuxPaneSnapshot {
    pub session: String,
    pub pane: String,
    pub width: u32,
    pub height: u32,
    pub content: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct TmuxSendResult {
    pub session: String,
    pub sent: bool,
    pub pane: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct TmuxSessionCreateResult {
    pub created: bool,
    pub session: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct TmuxSessionDeleteResult {
    pub deleted: bool,
    pub session: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TmuxKey {
    Enter,
    Escape,
    CtrlC,
    CtrlD,
    Tab,
    Up,
    Down,
    Left,
    Right,
}

impl TmuxKey {
    fn parse(raw: &str) -> Result<Self, String> {
        match raw {
            "enter" => Ok(Self::Enter),
            "escape" => Ok(Self::Escape),
            "ctrl_c" => Ok(Self::CtrlC),
            "ctrl_d" => Ok(Self::CtrlD),
            "tab" => Ok(Self::Tab),
            "up" => Ok(Self::Up),
            "down" => Ok(Self::Down),
            "left" => Ok(Self::Left),
            "right" => Ok(Self::Right),
            _ => Err(
                "tmux key must be one of enter, escape, ctrl_c, ctrl_d, tab, up, down, left, right"
                    .to_owned(),
            ),
        }
    }

    fn tmux_arg(self) -> &'static str {
        match self {
            Self::Enter => "Enter",
            Self::Escape => "Escape",
            Self::CtrlC => "C-c",
            Self::CtrlD => "C-d",
            Self::Tab => "Tab",
            Self::Up => "Up",
            Self::Down => "Down",
            Self::Left => "Left",
            Self::Right => "Right",
        }
    }
}

enum TmuxRunError {
    MissingBinary,
    TimedOut,
    Spawn(String),
    Wait(String),
}

struct TmuxCommandOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    truncated: bool,
    code: i32,
}

pub(crate) fn list_sessions() -> Result<TmuxSessionList, String> {
    match run_tmux(
        &["list-sessions", "-F", LIST_SESSIONS_FORMAT],
        TMUX_LIST_STDOUT_CAP,
    ) {
        Err(TmuxRunError::MissingBinary) => Ok(TmuxSessionList {
            tmux_available: false,
            sessions: Vec::new(),
        }),
        Err(error) => Err(format_run_error("list-sessions", error)),
        Ok(output) if output.code != 0 => Ok(TmuxSessionList {
            tmux_available: false,
            sessions: Vec::new(),
        }),
        Ok(output) if output.truncated => {
            Err("tmux list-sessions output exceeded the size limit".to_owned())
        }
        Ok(output) => {
            let mut sessions = parse_list_sessions(&output.stdout)?;
            fill_pane_counts(&mut sessions)?;
            Ok(TmuxSessionList {
                tmux_available: true,
                sessions,
            })
        }
    }
}

pub(crate) fn pane_snapshot(session: &str, lines: Option<u32>) -> Result<TmuxPaneSnapshot, String> {
    let session = validate_session_name(session)?;
    let (pane, width, height, _) = resolve_active_pane(session)?;
    let lines = clamp_capture_lines(lines);
    let start = format!("-{lines}");
    let captured = match run_tmux(
        &["capture-pane", "-e", "-t", &pane, "-p", "-S", &start],

        TMUX_CAPTURE_STDOUT_CAP,
    ) {
        Err(TmuxRunError::MissingBinary) => {
            return Err("tmux is not available".to_owned());
        }
        Err(error) => return Err(format_run_error("capture-pane", error)),
        Ok(output) if output.code != 0 => {
            return Err(format!("tmux session not found: {session}"));
        }
        Ok(output) => output,
    };
    let raw = String::from_utf8_lossy(&captured.stdout);
    let content = redact_bounded(&redact_secrets(&raw), TMUX_CONTENT_CHARS);
    Ok(TmuxPaneSnapshot {
        session: session.to_owned(),
        pane,
        width,
        height,
        content,
    })
}

pub(crate) fn send_keys(
    session: &str,
    text: Option<&str>,
    key: Option<&str>,
) -> Result<TmuxSendResult, String> {
    let session = validate_session_name(session)?;
    match (text, key) {
        (Some(text), None) => send_literal_text(session, text),
        (None, Some(key)) => send_named_key(session, key),
        _ => Err("tmux_send_keys requires exactly one of text or key".to_owned()),
    }
}

pub(crate) fn paste_text(session: &str, text: &str) -> Result<(), String> {
    let session = validate_session_name(session)?;
    let (pane, _, _, _) = resolve_active_pane(session)?;
    let buffer_path = std::env::temp_dir().join(format!("rpi-tmux-paste-{}.txt", Uuid::now_v7()));
    std::fs::write(&buffer_path, text)
        .map_err(|error| format!("write tmux paste buffer failed: {error}"))?;
    let path_str = buffer_path
        .to_str()
        .ok_or_else(|| "tmux paste buffer path is not UTF-8".to_owned())?;
    let buffer_name = format!("rpi-{}", Uuid::now_v7());
    let loaded = run_tmux(
        &["load-buffer", "-b", &buffer_name, path_str],
        TMUX_LIST_STDOUT_CAP,
    );
    let file_cleanup = remove_paste_buffer_file(&buffer_path);
    match loaded {
        Err(TmuxRunError::MissingBinary) => {
            return Err(merge_cleanup(
                "tmux is not available".to_owned(),
                file_cleanup,
            ));
        }
        Err(error) => {
            return Err(merge_cleanup(format_run_error("load-buffer", error), file_cleanup));
        }
        Ok(output) if output.code != 0 => {
            return Err(merge_cleanup(
                format!(
                    "tmux load-buffer failed: {session}: {}",
                    tmux_error_detail(&output)
                ),
                file_cleanup,
            ));
        }
        Ok(_) => {
            if let Err(error) = file_cleanup {
                eprintln!("tmux paste buffer cleanup failed: {error}");
            }
        }
    }
    match run_tmux(
        &["paste-buffer", "-d", "-b", &buffer_name, "-t", &pane],
        TMUX_LIST_STDOUT_CAP,
    ) {
        Err(TmuxRunError::MissingBinary) => Err(merge_cleanup(
            "tmux is not available".to_owned(),
            delete_tmux_named_buffer(&buffer_name),
        )),
        Err(error) => Err(merge_cleanup(
            format_run_error("paste-buffer", error),
            delete_tmux_named_buffer(&buffer_name),
        )),
        Ok(output) if output.code != 0 => Err(merge_cleanup(
            format!(
                "tmux paste-buffer failed: {session}: {}",
                tmux_error_detail(&output)
            ),
            delete_tmux_named_buffer(&buffer_name),
        )),
        Ok(_) => Ok(()),
    }
}

fn merge_cleanup(primary: String, cleanup: Result<(), String>) -> String {
    match cleanup {
        Ok(()) => primary,
        Err(error) => format!("{primary}; {error}"),
    }
}

fn remove_paste_buffer_file(path: &std::path::Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("remove tmux paste buffer failed: {error}")),
    }
}

fn delete_tmux_named_buffer(buffer_name: &str) -> Result<(), String> {
    match run_tmux(&["delete-buffer", "-b", buffer_name], TMUX_LIST_STDOUT_CAP) {
        Err(TmuxRunError::MissingBinary) => Err("tmux is not available".to_owned()),
        Err(error) => Err(format_run_error("delete-buffer", error)),
        Ok(output) if output.code != 0 => Err(format!(
            "tmux delete-buffer failed: {}",
            tmux_error_detail(&output)
        )),
        Ok(_) => Ok(()),
    }
}

pub(crate) fn create_session(session: &str) -> Result<TmuxSessionCreateResult, String> {
    let session = validate_session_name(session)?;
    match run_tmux(&["new-session", "-d", "-s", session], TMUX_LIST_STDOUT_CAP) {
        Err(TmuxRunError::MissingBinary) => Err("tmux is not available".to_owned()),
        Err(error) => Err(format_run_error("new-session", error)),
        Ok(output) if output.code != 0 => {
            let detail = tmux_error_detail(&output);
            if detail.contains("duplicate session") {
                Err(format!("tmux session already exists: {session}"))
            } else {
                Err(format!("tmux new-session failed: {session}"))
            }
        }
        Ok(_) => {
            purge_pane_history_for_session(session);
            Ok(TmuxSessionCreateResult {
                created: true,
                session: session.to_owned(),
            })
        }
    }
}

pub(crate) fn delete_session(session: &str) -> Result<TmuxSessionDeleteResult, String> {
    let session = validate_session_name(session)?;
    if session_has_active_terminals(session) {
        return Err("tmux session has active terminals".to_owned());
    }
    let selector = exact_session_selector(session);
    match run_tmux(&["kill-session", "-t", &selector], TMUX_LIST_STDOUT_CAP) {
        Err(TmuxRunError::MissingBinary) => Err("tmux is not available".to_owned()),
        Err(error) => Err(format_run_error("kill-session", error)),
        Ok(output) if output.code != 0 => Err(format!("tmux session not found: {session}")),
        Ok(_) => {
            purge_pane_history_for_session(session);
            Ok(TmuxSessionDeleteResult {
                deleted: true,
                session: session.to_owned(),
            })
        }
    }
}

pub(crate) fn session_has_active_terminals(session: &str) -> bool {
    SESSION_TERMINAL_COUNTS
        .lock()
        .get(session)
        .is_some_and(|count| *count > 0)
}

fn adjust_session_terminal_count(session: &str, add: bool) {
    let mut counts = SESSION_TERMINAL_COUNTS.lock();
    if add {
        *counts.entry(session.to_owned()).or_insert(0) += 1;
        return;
    }
    let Some(count) = counts.get_mut(session) else {
        return;
    };
    *count = count.saturating_sub(1);
    if *count == 0 {
        counts.remove(session);
    }
}

fn tmux_error_detail(output: &TmuxCommandOutput) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !stderr.trim().is_empty() {
        stderr.into_owned()
    } else {
        stdout.into_owned()
    }
}

fn send_literal_text(session: &str, text: &str) -> Result<TmuxSendResult, String> {
    if text.is_empty() {
        return Err("tmux send-keys text must be non-empty".to_owned());
    }
    if text.chars().count() > TMUX_SEND_TEXT_MAX {
        return Err("tmux send-keys text exceeds 4096 characters".to_owned());
    }
    let (pane, _, _, _) = resolve_active_pane(session)?;
    run_send(
        session,
        &pane,
        &["send-keys", "-t", &pane, "-l", "--", text],
    )
}

fn send_named_key(session: &str, key: &str) -> Result<TmuxSendResult, String> {
    let mapped = TmuxKey::parse(key)?;
    let (pane, _, _, _) = resolve_active_pane(session)?;
    run_send(
        session,
        &pane,
        &["send-keys", "-t", &pane, mapped.tmux_arg()],
    )
}

fn run_send(session: &str, pane: &str, args: &[&str]) -> Result<TmuxSendResult, String> {
    match run_tmux(args, TMUX_LIST_STDOUT_CAP) {
        Err(TmuxRunError::MissingBinary) => Err("tmux is not available".to_owned()),
        Err(error) => Err(format_run_error("send-keys", error)),
        Ok(output) if output.code != 0 => Err(format!("tmux session not found: {session}")),
        Ok(_) => Ok(TmuxSendResult {
            session: session.to_owned(),
            sent: true,
            pane: pane.to_owned(),
        }),
    }
}

fn exact_session_selector(session: &str) -> String {
    format!("={session}")
}

fn exact_active_target(session: &str) -> String {
    format!("={session}:")
}

fn require_session(session: &str) -> Result<(), String> {
    let selector = exact_session_selector(session);
    match run_tmux(&["has-session", "-t", &selector], TMUX_LIST_STDOUT_CAP) {
        Err(TmuxRunError::MissingBinary) => Err("tmux is not available".to_owned()),
        Err(error) => Err(format_run_error("has-session", error)),
        Ok(output) if output.code != 0 => Err(format!("tmux session not found: {session}")),
        Ok(_) => Ok(()),
    }
}

fn resolve_active_pane(session: &str) -> Result<(String, u32, u32, String), String> {
    require_session(session)?;
    let target = exact_active_target(session);
    match run_tmux(
        &["display-message", "-t", &target, "-p", PANE_META_FORMAT],
        TMUX_LIST_STDOUT_CAP,
    ) {
        Err(TmuxRunError::MissingBinary) => Err("tmux is not available".to_owned()),
        Err(error) => Err(format_run_error("display-message", error)),
        Ok(output) if output.code != 0 => Err(format!("tmux session not found: {session}")),
        Ok(output) if output.truncated => {
            Err("tmux display-message output exceeded the size limit".to_owned())
        }
        Ok(output) => parse_pane_meta(&output.stdout),
    }
}

pub(crate) fn validate_session_name(session: &str) -> Result<&str, String> {
    if session.is_empty() {
        return Err("tmux session name is empty".to_owned());
    }
    if session.len() > TMUX_SESSION_NAME_MAX {
        return Err("tmux session name exceeds 128 characters".to_owned());
    }
    if session.starts_with('-') {
        return Err("tmux session name must not start with '-'".to_owned());
    }
    if session
        .chars()
        .any(|ch| ch.is_whitespace() || ch.is_control())
    {
        return Err(
            "tmux session name must not contain whitespace or control characters".to_owned(),
        );
    }
    Ok(session)
}

pub(crate) fn clamp_capture_lines(lines: Option<u32>) -> u32 {
    lines.unwrap_or(80).clamp(1, 500)
}

pub(crate) fn parse_list_sessions(stdout: &[u8]) -> Result<Vec<TmuxSession>, String> {
    let text = std::str::from_utf8(stdout)
        .map_err(|_| "tmux list-sessions output is not valid UTF-8".to_owned())?;
    let mut sessions = Vec::new();
    for line in text.split('\n') {
        if line.is_empty() {
            continue;
        }
        sessions.push(parse_list_session_line(line)?);
    }
    Ok(sessions)
}

fn parse_list_session_line(line: &str) -> Result<TmuxSession, String> {
    let mut fields = line.split('\t');
    let name = fields
        .next()
        .ok_or_else(|| "tmux list-sessions record is missing session_name".to_owned())?;
    let windows = parse_u32_field(fields.next(), "session_windows")?;
    let attached_raw = parse_i64_field(fields.next(), "session_attached")?;
    let created_epoch = parse_i64_field(fields.next(), "session_created")?;
    let pane_count = match fields.next() {
        Some(value) => parse_u32_field(Some(value), "session_panes")?,
        None => 0,
    };
    if fields.next().is_some() {
        return Err("tmux list-sessions record has extra fields".to_owned());
    }
    Ok(TmuxSession {
        name: name.to_owned(),
        windows,
        attached: attached_raw != 0,
        created_epoch,
        pane_count,
    })
}

fn fill_pane_counts(sessions: &mut [TmuxSession]) -> Result<(), String> {
    let output = match run_tmux(
        &["list-panes", "-a", "-F", PANE_LIST_FORMAT],
        TMUX_LIST_STDOUT_CAP,
    ) {
        Err(TmuxRunError::MissingBinary) => {
            return Err("tmux is not available".to_owned());
        }
        Err(error) => return Err(format_run_error("list-panes", error)),
        Ok(output) if output.code != 0 => {
            return Err("tmux list-panes failed".to_owned());
        }
        Ok(output) if output.truncated => {
            return Err("tmux list-panes output exceeded the size limit".to_owned());
        }
        Ok(output) => output,
    };
    let text = std::str::from_utf8(&output.stdout)
        .map_err(|_| "tmux list-panes output is not valid UTF-8".to_owned())?;
    let mut counts: BTreeMap<&str, u32> = BTreeMap::new();
    for line in text.split('\n') {
        if line.is_empty() {
            continue;
        }
        *counts.entry(line).or_insert(0) += 1;
    }
    for session in sessions.iter_mut() {
        session.pane_count = counts.get(session.name.as_str()).copied().unwrap_or(0);
    }
    Ok(())
}

fn parse_pane_meta(stdout: &[u8]) -> Result<(String, u32, u32, String), String> {
    let text = std::str::from_utf8(stdout)
        .map_err(|_| "tmux display-message output is not valid UTF-8".to_owned())?;
    let line = text.trim_end_matches(['\n', '\r']);
    let mut fields = line.split('\t');
    let pane = fields
        .next()
        .ok_or_else(|| "tmux display-message record is missing pane_id".to_owned())?;
    let width = parse_u32_field(fields.next(), "pane_width")?;
    let height = parse_u32_field(fields.next(), "pane_height")?;
    let daemon_epoch = fields
        .next()
        .ok_or_else(|| "tmux display-message record is missing start_time".to_owned())?;
    if fields.next().is_some() || pane.is_empty() || daemon_epoch.is_empty() {
        return Err("tmux display-message record is malformed".to_owned());
    }
    Ok((pane.to_owned(), width, height, daemon_epoch.to_owned()))
}

fn parse_u32_field(value: Option<&str>, field: &str) -> Result<u32, String> {
    let value = value.ok_or_else(|| format!("tmux record is missing {field}"))?;
    value
        .parse::<u32>()
        .map_err(|_| format!("tmux record has a non-integer {field}"))
}

fn parse_i64_field(value: Option<&str>, field: &str) -> Result<i64, String> {
    let value = value.ok_or_else(|| format!("tmux record is missing {field}"))?;
    value
        .parse::<i64>()
        .map_err(|_| format!("tmux record has a non-integer {field}"))
}

fn format_run_error(command: &str, error: TmuxRunError) -> String {
    match error {
        TmuxRunError::MissingBinary => "tmux is not available".to_owned(),
        TmuxRunError::TimedOut => format!("tmux {command} timed out"),
        TmuxRunError::Spawn(error) => format!("tmux {command} spawn failed: {error}"),
        TmuxRunError::Wait(error) => format!("tmux {command} wait failed: {error}"),
    }
}

fn run_tmux(args: &[&str], max_stdout: usize) -> Result<TmuxCommandOutput, TmuxRunError> {
    let mut command = Command::new("tmux");
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(TmuxRunError::MissingBinary);
        }
        Err(error) => return Err(TmuxRunError::Spawn(error.to_string())),
    };
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| TmuxRunError::Wait("tmux stdout pipe unavailable".to_owned()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| TmuxRunError::Wait("tmux stderr pipe unavailable".to_owned()))?;
    let stdout_thread = std::thread::spawn(move || read_capped(stdout, max_stdout));
    let stderr_thread = std::thread::spawn(move || read_capped(stderr, TMUX_STDERR_CAP));
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() >= TMUX_TIMEOUT => {
                let _ = child.kill();
                if wait_std_child_bounded(&mut child, TMUX_TIMEOUT) {
                    let _ = stdout_thread.join();
                    let _ = stderr_thread.join();
                }
                return Err(TmuxRunError::TimedOut);
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(error) => {
                let _ = child.kill();
                if wait_std_child_bounded(&mut child, TMUX_TIMEOUT) {
                    let _ = stdout_thread.join();
                    let _ = stderr_thread.join();
                }
                return Err(TmuxRunError::Wait(error.to_string()));
            }
        }
    };
    let stdout_read = stdout_thread
        .join()
        .map_err(|_| TmuxRunError::Wait("tmux stdout reader panicked".to_owned()))?;
    let stderr_read = stderr_thread
        .join()
        .map_err(|_| TmuxRunError::Wait("tmux stderr reader panicked".to_owned()))?;
    let code = status.code().unwrap_or(1);
    Ok(TmuxCommandOutput {
        stdout: stdout_read.0,
        stderr: stderr_read.0,
        truncated: stdout_read.1,
        code,
    })
}

fn read_capped(mut reader: impl Read, cap: usize) -> (Vec<u8>, bool) {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let mut truncated = false;
    loop {
        match reader.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => {
                if buf.len() >= cap {
                    truncated = true;
                    continue;
                }
                let take = n.min(cap - buf.len());
                buf.extend_from_slice(&tmp[..take]);
                if take < n {
                    truncated = true;
                }
            }
            Err(_) => break,
        }
    }
    (buf, truncated)
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum TmuxTerminalEvent {
    TmuxTerminalOutput {
        #[serde(rename = "terminalId")]
        terminal_id: String,
        data: String,
    },
    TmuxTerminalExit {
        #[serde(rename = "terminalId")]
        terminal_id: String,
        code: i32,
    },
    TmuxTerminalHistory {
        #[serde(rename = "terminalId")]
        terminal_id: String,
        #[serde(rename = "paneKey")]
        pane_key: String,
        #[serde(rename = "historyKey")]
        history_key: String,
        data: String,
        #[serde(rename = "final")]
        is_final: bool,
    },
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TmuxTerminalAttachResult {
    pub terminal_id: String,
    pub session: String,
    pub cols: u32,
    pub rows: u32,
    pub pane_key: String,
    pub history_key: String,
    pub history_bytes: u32,
    pub history_complete: bool,
}

struct HistoryAttachMeta {
    pane_key: String,
    history_key: String,
    history_bytes: u32,
    history_complete: bool,
    replay: Option<Arc<Vec<u8>>>,
}

struct LiveTerminal {
    owner: Uuid,
    session: String,
    writes: mpsc::Sender<Vec<u8>>,
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    killer: Box<dyn portable_pty::ChildKiller + Send + Sync>,
    applied_size: Option<(u16, u16)>,
}

struct InflightAttach {
    token: Uuid,
    killer: Option<Box<dyn portable_pty::ChildKiller + Send + Sync>>,
}

struct RegistryState {
    terminals: HashMap<String, LiveTerminal>,
    reserved: usize,
    closed_owners: HashSet<Uuid>,
    closed_owner_order: VecDeque<Uuid>,
    inflight: HashMap<Uuid, Vec<InflightAttach>>,
}

impl RegistryState {
    fn occupy_slot(&mut self) -> Result<(), String> {
        if self.reserved >= TMUX_TERMINAL_MAX {
            return Err(format!(
                "too many live tmux terminals (limit {TMUX_TERMINAL_MAX})"
            ));
        }
        self.reserved += 1;
        Ok(())
    }

    fn release_slot(&mut self) {
        self.reserved = self.reserved.saturating_sub(1);
    }

    fn take_terminal(&mut self, terminal_id: &str) -> Option<LiveTerminal> {
        let terminal = self.terminals.remove(terminal_id)?;
        self.release_slot();
        adjust_session_terminal_count(&terminal.session, false);
        Some(terminal)
    }

    fn inflight_has(&self, owner: Uuid, token: Uuid) -> bool {
        self.inflight
            .get(&owner)
            .is_some_and(|list| list.iter().any(|item| item.token == token))
    }

    fn owner_is_busy(&self, owner: Uuid) -> bool {
        self.inflight.contains_key(&owner)
            || self.terminals.values().any(|terminal| terminal.owner == owner)
    }

    fn mark_closed(&mut self, owner: Uuid) {
        if self.closed_owners.insert(owner) {
            self.closed_owner_order.push_back(owner);
        }
        self.evict_surplus_closed_owners();
    }

    fn evict_surplus_closed_owners(&mut self) {
        let mut scanned = 0usize;
        let max_scan = self.closed_owner_order.len();
        while self.closed_owners.len() > TMUX_TERMINAL_CLOSED_OWNERS_MAX && scanned < max_scan {
            scanned += 1;
            let Some(old) = self.closed_owner_order.pop_front() else {
                break;
            };
            if self.owner_is_busy(old) {
                self.closed_owner_order.push_back(old);
                continue;
            }
            self.closed_owners.remove(&old);
        }
    }

    fn remove_inflight(&mut self, owner: Uuid, token: Uuid) -> bool {
        self.take_inflight(owner, token).is_some()
    }

    fn take_inflight(&mut self, owner: Uuid, token: Uuid) -> Option<InflightAttach> {
        let list = self.inflight.get_mut(&owner)?;
        let pos = list.iter().position(|item| item.token == token)?;
        let item = list.remove(pos);
        if list.is_empty() {
            self.inflight.remove(&owner);
        }
        Some(item)
    }

    fn inflight_killer(
        &mut self,
        owner: Uuid,
        token: Uuid,
        killer: Box<dyn portable_pty::ChildKiller + Send + Sync>,
    ) {
        if let Some(list) = self.inflight.get_mut(&owner)
            && let Some(item) = list.iter_mut().find(|item| item.token == token)
        {
            item.killer = Some(killer);
        }
    }
}

pub(crate) struct TmuxTerminalRegistry {
    inner: Mutex<RegistryState>,
}

impl Default for TmuxTerminalRegistry {
    fn default() -> Self {
        Self {
            inner: Mutex::new(RegistryState {
                terminals: HashMap::new(),
                reserved: 0,
                closed_owners: HashSet::new(),
                closed_owner_order: VecDeque::new(),
                inflight: HashMap::new(),
            }),
        }
    }
}

impl TmuxTerminalRegistry {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub(crate) fn live_count(&self) -> usize {
        self.inner.lock().terminals.len()
    }

    pub(crate) fn event_queue_capacity() -> usize {
        TMUX_TERMINAL_EVENT_QUEUE
    }

    pub(crate) fn write_queue_capacity() -> usize {
        TMUX_TERMINAL_WRITE_QUEUE
    }

    #[cfg(test)]
    pub(crate) fn inflight_count(&self) -> usize {
        self.inner
            .lock()
            .inflight
            .values()
            .map(|list| list.len())
            .sum()
    }

    fn take_live_terminal(&self, terminal_id: &str) -> Option<LiveTerminal> {
        let terminal = self.inner.lock().take_terminal(terminal_id)?;
        let _ = set_session_window_size_largest(&terminal.session);
        Some(terminal)
    }

    fn abandon_inflight(&self, owner: Uuid, token: Uuid) {
        let mut inner = self.inner.lock();
        if inner.remove_inflight(owner, token) {
            inner.release_slot();
        }
    }

    fn abandon_or_kill_inflight(&self, owner: Uuid, token: Uuid) {
        let killer = {
            let mut inner = self.inner.lock();
            let Some(item) = inner.take_inflight(owner, token) else {
                return;
            };
            inner.release_slot();
            item.killer
        };
        if let Some(mut killer) = killer {
            let _ = killer.kill();
        }
    }

    pub(crate) fn kill_owner(&self, owner: Uuid) {
        let (terminals, inflight) = {
            let mut inner = self.inner.lock();
            inner.mark_closed(owner);
            let ids: Vec<String> = inner
                .terminals
                .iter()
                .filter(|(_, terminal)| terminal.owner == owner)
                .map(|(id, _)| id.clone())
                .collect();
            let mut terminals = Vec::new();
            for id in ids {
                if let Some(terminal) = inner.take_terminal(&id) {
                    terminals.push(terminal);
                }
            }
            let inflight = inner.inflight.remove(&owner).unwrap_or_default();
            for _ in 0..inflight.len() {
                inner.release_slot();
            }
            inner.evict_surplus_closed_owners();
            (terminals, inflight)
        };
        for mut terminal in terminals {
            let _ = terminal.killer.kill();
            let _ = set_session_window_size_largest(&terminal.session);
        }
        for mut pending in inflight {
            if let Some(mut killer) = pending.killer {
                let _ = killer.kill();
            }
        }
    }

    pub(crate) async fn attach(
        self: &Arc<Self>,
        owner: Uuid,
        session: &str,
        cols: u32,
        rows: u32,
        full_history: bool,
        cached_history_key: Option<&str>,
    ) -> Result<(TmuxTerminalAttachResult, mpsc::Receiver<TmuxTerminalEvent>), String>
    {
        let session = validate_session_name(session)?.to_owned();
        let cols = clamp_terminal_cols(cols);
        let rows = clamp_terminal_rows(rows);
        let token = Uuid::now_v7();
        {
            let mut inner = self.inner.lock();
            if inner.closed_owners.contains(&owner) {
                return Err("tmux terminal owner is closed".to_owned());
            }
            inner.occupy_slot()?;
            inner
                .inflight
                .entry(owner)
                .or_default()
                .push(InflightAttach {
                    token,
                    killer: None,
                });
        }
        let registry = Arc::clone(self);
        let spawn_session = session.clone();
        let cached_history_key = cached_history_key.map(str::to_owned);
        let (ready_tx, ready_rx) = oneshot::channel();
        tokio::task::spawn_blocking(move || {
            let spawned = match require_session(&spawn_session).and_then(|_| {
                spawn_pty_attach(
                    &spawn_session,
                    cols,
                    rows,
                    owner,
                    full_history,
                    cached_history_key.as_deref(),
                )
            }) {
                Ok(pair) => pair,
                Err(error) => {
                    registry.abandon_inflight(owner, token);
                    let _ = ready_tx.send(Err(error));
                    return;
                }
            };
            let (spawned, history) = spawned;
            let rejected = {
                let mut inner = registry.inner.lock();
                if inner.closed_owners.contains(&owner) || !inner.inflight_has(owner, token) {
                    if inner.remove_inflight(owner, token) {
                        inner.release_slot();
                    }
                    true
                } else {
                    if let Some(killer) = spawned.killer.as_ref() {
                        inner.inflight_killer(owner, token, killer.clone_killer());
                    }
                    false
                }
            };
            if rejected {
                drop_spawned_pty_blocking(spawned);
                let _ = ready_tx.send(Err("tmux terminal owner is closed".to_owned()));
                return;
            }
            let (commit_tx, commit_rx) = oneshot::channel();
            if let Err(Ok((spawned, _history, _commit_tx))) =
                ready_tx.send(Ok((spawned, history, commit_tx)))
            {
                registry.abandon_or_kill_inflight(owner, token);
                drop_spawned_pty_blocking(spawned);
                return;
            }
            if commit_rx.blocking_recv().is_err() {
                registry.abandon_or_kill_inflight(owner, token);
            }
        });
        #[cfg(test)]
        pause_attach_before_recv(owner).await;
        let (mut spawned, history, commit_tx) = match ready_rx.await {
            Ok(Ok(tuple)) => tuple,
            Ok(Err(error)) => return Err(error),
            Err(_) => {
                self.abandon_or_kill_inflight(owner, token);
                return Err("tmux terminal attach failed".to_owned());
            }
        };
        let terminal_id = Uuid::now_v7().to_string();
        let (events_tx, events_rx) = mpsc::channel(TMUX_TERMINAL_EVENT_QUEUE);
        let (raw_tx, raw_rx) = mpsc::channel::<Vec<u8>>(TMUX_TERMINAL_RAW_QUEUE);
        let (write_tx, mut write_rx) = mpsc::channel::<Vec<u8>>(TMUX_TERMINAL_WRITE_QUEUE);
        let (exit_tx, exit_rx) = oneshot::channel();
        let Some(master_box) = spawned.master.take() else {
            drop(commit_tx);
            drop_spawned_pty(spawned).await?;
            return Err("tmux terminal pty master missing".to_owned());
        };
        let master = Arc::new(Mutex::new(master_box));
        let Some(killer) = spawned.killer.as_ref().map(|killer| killer.clone_killer()) else {
            drop(commit_tx);
            drop_spawned_pty(spawned).await?;
            return Err("tmux terminal child killer missing".to_owned());
        };
        let inserted = {
            let mut inner = self.inner.lock();
            let still_inflight = inner.remove_inflight(owner, token);
            if inner.closed_owners.contains(&owner) {
                if still_inflight {
                    inner.release_slot();
                }
                false
            } else {
                inner.terminals.insert(
                    terminal_id.clone(),
                    LiveTerminal {
                        owner,
                        session: session.clone(),
                        writes: write_tx,
                        master: Arc::clone(&master),
                        killer,
                        applied_size: Some((cols, rows)),
                    },
                );
                adjust_session_terminal_count(&session, true);
                true
            }
        };
        let mut inserted_guard = UncommittedInsert {
            registry: Arc::clone(self),
            terminal_id: terminal_id.clone(),
            disarmed: !inserted,
        };
        if !inserted {
            drop(commit_tx);
            drop_spawned_pty(spawned).await?;
            return Err("tmux terminal owner is closed".to_owned());
        }
        #[cfg(test)]
        pause_attach_after_insert(owner).await;
        let (mut child, mut reader, mut writer) = match (
            spawned.child.take(),
            spawned.reader.take(),
            spawned.writer.take(),
        ) {
            (Some(child), Some(reader), Some(writer)) => (child, reader, writer),
            (child, reader, writer) => {
                spawned.child = child;
                spawned.reader = reader;
                spawned.writer = writer;
                drop(commit_tx);
                drop_spawned_pty(spawned).await?;
                return Err("tmux terminal pty handles missing".to_owned());
            }
        };
        #[cfg(test)]
        if STALL_WRITER_OWNERS.lock().iter().any(|id| *id == owner) {
            drop(writer);
            std::mem::forget(write_rx);
        } else {
            start_terminal_writer(write_rx, writer);
        }
        #[cfg(not(test))]
        start_terminal_writer(write_rx, writer);
        // Replay runs concurrently with the RPC response: the consumer only
        // starts draining after attach() returns, so an inline await would
        // deadlock on a full queue once history outgrew the event queue. A
        // dead consumer closes the channel and ends the task immediately.
        let replay_events = events_tx.clone();
        let replay_terminal_id = terminal_id.clone();
        let replay_pane_key = history.pane_key.clone();
        let replay_history_key = history.history_key.clone();
        let replay_bytes = history.replay.clone().unwrap_or_default();
        tokio::task::spawn(async move {
            enqueue_history_replay(
                &replay_events,
                &replay_terminal_id,
                &replay_pane_key,
                &replay_history_key,
                &replay_bytes,
            )
            .await;
        });
        let exit_id = terminal_id.clone();
        let registry = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            let mut buffer = [0_u8; TMUX_TERMINAL_PTY_CHUNK];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => {
                        if raw_tx.blocking_send(buffer[..read].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
            drop(raw_tx);
            let code = wait_pty_child_bounded(&mut *child, TMUX_TIMEOUT).unwrap_or(-1);
            let _ = exit_tx.send(code);
            let _ = registry.take_live_terminal(&exit_id);
        });
        spawned.commit();
        let _ = commit_tx.send(());
        tokio::spawn(coalesce_terminal_output(
            raw_rx,
            events_tx,
            terminal_id.clone(),
            exit_rx,
        ));
        inserted_guard.disarm();
        Ok((
            TmuxTerminalAttachResult {
                terminal_id,
                session,
                cols: u32::from(cols),
                rows: u32::from(rows),
                pane_key: history.pane_key,
                history_key: history.history_key,
                history_bytes: history.history_bytes,
                history_complete: history.history_complete,
            },
            events_rx,
        ))
    }

    pub(crate) async fn input(
        &self,
        owner: Uuid,
        terminal_id: &str,
        data: &str,
    ) -> Result<(), String> {
        let bytes = decode_terminal_input(data)?;
        let writes = {
            let inner = self.inner.lock();
            let terminal = inner
                .terminals
                .get(terminal_id)
                .ok_or_else(|| format!("tmux terminal not found: {terminal_id}"))?;
            if terminal.owner != owner {
                return Err(format!("tmux terminal not found: {terminal_id}"));
            }
            terminal.writes.clone()
        };
        match tokio::time::timeout(TMUX_TERMINAL_WRITE_SEND, writes.send(bytes)).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(format!("tmux terminal write failed: {terminal_id}")),
            Err(_) => {
                let _ = self.detach(owner, terminal_id);
                Err(format!("{TMUX_TERMINAL_WRITE_TIMED_OUT}: {terminal_id}"))
            }
        }
    }

    pub(crate) async fn resize(
        self: &Arc<Self>,
        owner: Uuid,
        terminal_id: &str,
        cols: u32,
        rows: u32,
        fit_window: bool,
    ) -> Result<(), String> {
        let cols = clamp_terminal_cols(cols);
        let rows = clamp_terminal_rows(rows);
        let terminal_id = terminal_id.to_owned();
        let (session, master, unchanged) = {
            let inner = self.inner.lock();
            let terminal = inner
                .terminals
                .get(terminal_id.as_str())
                .ok_or_else(|| format!("tmux terminal not found: {terminal_id}"))?;
            if terminal.owner != owner {
                return Err(format!("tmux terminal not found: {terminal_id}"));
            }
            let unchanged = !fit_window && terminal.applied_size == Some((cols, rows));
            (terminal.session.clone(), Arc::clone(&terminal.master), unchanged)
        };
        if unchanged {
            return Ok(());
        }
        let registry = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            let client_tty = {
                let master = master.lock();
                master
                    .resize(PtySize {
                        rows: rows as u16,
                        cols,
                        pixel_width: 0,
                        pixel_height: 0,
                    })
                    .map_err(|error| format!("tmux terminal resize failed: {error}"))?;
                unix_pty_tty_name(master.as_ref())
            };
            if !registry.inner.lock().terminals.contains_key(terminal_id.as_str()) {
                return Ok(());
            }
            apply_tmux_client_size(&session, client_tty.as_deref(), cols, rows, fit_window)?;
            let mut inner = registry.inner.lock();
            if let Some(terminal) = inner.terminals.get_mut(&terminal_id) {
                terminal.applied_size = Some((cols, rows));
            }
            Ok(())
        })
        .await
        .map_err(|error| format!("tmux terminal resize failed: {error}"))?
    }

    pub(crate) fn detach(&self, owner: Uuid, terminal_id: &str) -> Result<(), String> {
        let mut terminal = {
            let mut inner = self.inner.lock();
            let owner_ok = inner
                .terminals
                .get(terminal_id)
                .is_some_and(|terminal| terminal.owner == owner);
            if !owner_ok {
                return Err(format!("tmux terminal not found: {terminal_id}"));
            }
            inner
                .take_terminal(terminal_id)
                .ok_or_else(|| format!("tmux terminal not found: {terminal_id}"))?
        };
        let _ = terminal.killer.kill();
        set_session_window_size_largest(&terminal.session)
    }

    fn kill_terminal(&self, terminal_id: &str) {
        if let Some(mut terminal) = self.take_live_terminal(terminal_id) {
            let _ = terminal.killer.kill();
        }
    }
}

struct UncommittedInsert {
    registry: Arc<TmuxTerminalRegistry>,
    terminal_id: String,
    disarmed: bool,
}

impl UncommittedInsert {
    fn disarm(&mut self) {
        self.disarmed = true;
    }
}

impl Drop for UncommittedInsert {
    fn drop(&mut self) {
        if self.disarmed {
            return;
        }
        if let Some(mut terminal) = self.registry.take_live_terminal(&self.terminal_id) {
            let _ = terminal.killer.kill();
        }
    }
}

thread_local! {
    static REAP_SPAWNED_PTY_INLINE: Cell<bool> = const { Cell::new(false) };
}

fn drop_spawned_pty_blocking(spawned: SpawnedPty) {
    REAP_SPAWNED_PTY_INLINE.set(true);
    drop(spawned);
    REAP_SPAWNED_PTY_INLINE.set(false);
}

async fn drop_spawned_pty(spawned: SpawnedPty) -> Result<(), String> {
    tokio::task::spawn_blocking(move || drop_spawned_pty_blocking(spawned))
        .await
        .map_err(|error| format!("tmux terminal attach failed: {error}"))
}

struct SpawnedPty {
    master: Option<Box<dyn MasterPty + Send>>,
    writer: Option<Box<dyn Write + Send>>,
    reader: Option<Box<dyn Read + Send>>,
    killer: Option<Box<dyn ChildKiller + Send + Sync>>,
    child: Option<Box<dyn portable_pty::Child + Send + Sync>>,
    committed: bool,
}

impl SpawnedPty {
    fn commit(&mut self) {
        self.committed = true;
    }
}

impl Drop for SpawnedPty {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        let killer = self.killer.take();
        let Some(child) = self.child.take() else {
            return;
        };
        let reap = move || reap_uncommitted_child(killer, child);
        if REAP_SPAWNED_PTY_INLINE.get() {
            reap();
            return;
        }
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                let _ = handle.spawn_blocking(move || {
                    REAP_SPAWNED_PTY_INLINE.set(true);
                    reap();
                });
            }
            Err(_) => reap(),
        }
    }
}

fn log_wait_timeout_after_kill(pid: Option<u32>) {
    let pid = pid.map(|id| id.to_string()).unwrap_or_else(|| "-".to_owned());
    eprintln!(
        "tmux_terminal category=wait_timeout_after_kill pid={pid} elapsed_ms={}",
        TMUX_TIMEOUT.as_millis()
    );
}

fn wait_std_child_bounded(child: &mut std::process::Child, timeout: Duration) -> bool {
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return true,
            Ok(None) if started.elapsed() >= timeout => {
                log_wait_timeout_after_kill(Some(child.id()));
                return false;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
        }
    }
}

fn wait_pty_child_bounded(child: &mut dyn portable_pty::Child, timeout: Duration) -> Option<i32> {
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status.exit_code() as i32),
            Err(_) => return Some(-1),
            Ok(None) if started.elapsed() >= timeout => {
                log_wait_timeout_after_kill(child.process_id());
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
        }
    }
}

fn reap_uncommitted_child(
    killer: Option<Box<dyn ChildKiller + Send + Sync>>,
    mut child: Box<dyn portable_pty::Child + Send + Sync>,
) {
    if let Some(mut killer) = killer {
        let _ = killer.kill();
    }
    if let Ok(Some(_)) = child.try_wait() {
        return;
    }
    if let Some(pid) = child.process_id() {
        let _ = Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    // SIGKILL delivered: try_wait until TMUX_TIMEOUT, then leak the wait
    // rather than pin a blocking worker (and runtime Drop) forever.
    let _ = wait_pty_child_bounded(&mut *child, TMUX_TIMEOUT);
}

fn start_terminal_writer(
    mut write_rx: mpsc::Receiver<Vec<u8>>,
    mut writer: Box<dyn Write + Send>,
) {
    tokio::task::spawn_blocking(move || {
        while let Some(bytes) = write_rx.blocking_recv() {
            if writer.write_all(&bytes).and_then(|()| writer.flush()).is_err() {
                break;
            }
        }
    });
}

fn spawn_pty_attach(
    session: &str,
    cols: u16,
    rows: u16,
    #[cfg_attr(not(test), allow(unused_variables))] owner: Uuid,
    full_history: bool,
    cached_history_key: Option<&str>,
) -> Result<(SpawnedPty, HistoryAttachMeta), String> {
    let (pane_key, _, _, daemon_epoch) = resolve_active_pane(session)?;
    resize_session_window(session, cols, rows)?;
    let history_key = pane_history_key(&daemon_epoch, session, &pane_key);
    let mut replay = None;
    let mut history_bytes = 0u32;
    let mut history_complete = true;
    if !(cached_history_key == Some(history_key.as_str()) && pane_history_lookup(&history_key).is_some()) {
        let captured = match pane_history_lookup(&history_key) {
            Some(stored) if full_history => stored,
            _ => {
                let (captured, truncated) = capture_pane_ansi(&pane_key)?;
                history_complete = !truncated;
                let stored = Arc::new(captured);
                if full_history {
                    pane_history_store(history_key.clone(), session.to_owned(), Arc::clone(&stored));
                }
                stored
            }
        };
        history_bytes = u32::try_from(captured.len()).unwrap_or(u32::MAX);
        if !captured.is_empty() {
            replay = Some(captured);
        }
    }
    let pair = native_pty_system()
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|error| format!("tmux terminal pty open failed: {error}"))?;
    let command = attach_session_command(session);
    let child = pair
        .slave
        .spawn_command(command)
        .map_err(|error| format!("tmux attach-session spawn failed: {error}"))?;
    let killer = child.clone_killer();
    drop(pair.slave);
    #[cfg(test)]
    if let Some(pid) = child.process_id() {
        let mut rows = LAST_SPAWNED_CHILD.lock();
        if let Some(entry) = rows.iter_mut().find(|(row_owner, _)| *row_owner == owner) {
            entry.1 = pid;
        } else {
            rows.push((owner, pid));
        }
    }
    let mut spawned = SpawnedPty {
        master: Some(pair.master),
        writer: None,
        reader: None,
        killer: Some(killer),
        child: Some(child),
        committed: false,
    };
    #[cfg(test)]
    if FAIL_SETUP_OWNER.lock().as_ref() == Some(&owner) {
        return Err("tmux terminal pty setup failed".to_owned());
    }
    let writer = spawned
        .master
        .as_ref()
        .ok_or_else(|| "tmux terminal pty master missing".to_owned())?
        .take_writer()
        .map_err(|error| format!("tmux terminal pty writer failed: {error}"))?;
    let reader = spawned
        .master
        .as_ref()
        .ok_or_else(|| "tmux terminal pty master missing".to_owned())?
        .try_clone_reader()
        .map_err(|error| format!("tmux terminal pty reader failed: {error}"))?;
    spawned.writer = Some(writer);
    spawned.reader = Some(reader);
    Ok((
        spawned,
        HistoryAttachMeta {
            pane_key,
            history_key,
            history_bytes,
            history_complete,
            replay,
        },
    ))
}

fn terminal_history_capture_args(pane: &str) -> [&str; 9] {
    [
        "capture-pane",
        "-e",
        "-p",
        "-t",
        pane,
        "-S",
        TMUX_TERMINAL_CAPTURE_START,
        "-E",
        "-1",
    ]
}

fn capture_pane_ansi(pane: &str) -> Result<(Vec<u8>, bool), String> {
    let captured = run_tmux(
        &terminal_history_capture_args(pane),
        TMUX_TERMINAL_CAPTURE_STDOUT_CAP,
    )
    .map_err(|error| format_run_error("capture-pane", error))?;
    if captured.code != 0 {
        return Err("tmux terminal history capture failed".to_owned());
    }
    Ok((captured.stdout, captured.truncated))
}


#[cfg(test)]
async fn pause_attach_before_recv(owner: Uuid) {
    if HOLD_RECV_OWNER.lock().as_ref() != Some(&owner) {
        return;
    }
    ATTACH_BEFORE_RECV_READY.store(true, Ordering::SeqCst);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !ATTACH_HOLD_RELEASE.load(Ordering::SeqCst) {
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[cfg(test)]
async fn pause_attach_after_insert(owner: Uuid) {
    if HOLD_AFTER_INSERT_OWNER.lock().as_ref() != Some(&owner) {
        return;
    }
    ATTACH_AFTER_INSERT_READY.store(true, Ordering::SeqCst);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !ATTACH_AFTER_INSERT_RELEASE.load(Ordering::SeqCst) {
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn attach_session_command(session: &str) -> CommandBuilder {
    let mut command = CommandBuilder::new("tmux");
    command.env_remove("TMUX");
    command.env("TERM", TMUX_TERMINAL_TERM);
    if let Ok(dir) = std::env::current_dir() {
        command.cwd(dir);
    }
    command.arg("attach-session");
    command.arg("-t");
    command.arg(exact_session_selector(session));
    command
}

fn unix_pty_tty_name(master: &dyn MasterPty) -> Option<String> {
    #[cfg(unix)]
    {
        master
            .tty_name()
            .and_then(|path| path.into_os_string().into_string().ok())
    }
    #[cfg(not(unix))]
    {
        let _ = master;
        None
    }
}

fn set_window_size_option(target: &str, value: &str) -> Result<(), String> {
    match run_tmux(
        &["set-option", "-t", target, "window-size", value],
        TMUX_LIST_STDOUT_CAP,
    ) {
        Err(error) => Err(format_run_error("set-option", error)),
        Ok(output) if output.code != 0 => Err(format!(
            "tmux set-option window-size failed (code {}) {}",
            output.code,
            tmux_error_detail(&output).trim()
        )),
        Ok(_) => Ok(()),
    }
}

fn set_session_window_size_largest(session: &str) -> Result<(), String> {
    set_window_size_option(&exact_active_target(session), "largest")
}

fn window_size_option(session: &str) -> Option<String> {
    let target = exact_active_target(session);
    let output = run_tmux(
        &["show-options", "-t", &target, "-v", "window-size"],
        TMUX_LIST_STDOUT_CAP,
    )
    .ok()?;
    if output.code != 0 {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

fn restore_window_size_option(session: &str, previous: Option<&str>) -> Result<(), String> {
    set_window_size_option(&exact_active_target(session), previous.unwrap_or("largest"))
}

fn resize_session_window(session: &str, cols: u16, rows: u16) -> Result<(), String> {
    let target = exact_active_target(session);
    let previous = window_size_option(session);
    set_window_size_option(&target, "manual")?;
    let width = cols.to_string();
    let height = rows.to_string();
    match run_tmux(
        &[
            "resize-window",
            "-t",
            &target,
            "-x",
            &width,
            "-y",
            &height,
        ],
        TMUX_LIST_STDOUT_CAP,
    ) {
        Err(error) => {
            let restore = restore_window_size_option(session, previous.as_deref());
            let primary = format_run_error("resize-window", error);
            match restore {
                Ok(()) => Err(primary),
                Err(restore_error) => Err(format!("{primary}; {restore_error}")),
            }
        }
        Ok(output) if output.code == 0 => Ok(()),
        Ok(output) => {
            let primary = format!(
                "tmux resize-window failed (code {}) {}",
                output.code,
                tmux_error_detail(&output).trim()
            );
            match restore_window_size_option(session, previous.as_deref()) {
                Ok(()) => Err(primary),
                Err(restore_error) => Err(format!("{primary}; {restore_error}")),
            }
        }
    }
}

enum TmuxClientRefresh {
    Refreshed,
    ClientGone,
}

fn parse_refresh_client_gone(detail: &str) -> bool {
    let text = detail.trim();
    text.starts_with("can't find client")
}

fn refresh_tmux_client(tty: &str) -> Result<TmuxClientRefresh, String> {
    match run_tmux(&["refresh-client", "-t", tty], TMUX_LIST_STDOUT_CAP) {
        Err(error) => Err(format_run_error("refresh-client", error)),
        Ok(output) if output.code == 0 => Ok(TmuxClientRefresh::Refreshed),
        Ok(output) => {
            let detail = tmux_error_detail(&output);
            if parse_refresh_client_gone(&detail) {
                Ok(TmuxClientRefresh::ClientGone)
            } else {
                Err(format!(
                    "tmux refresh-client failed (code {}) {}",
                    output.code,
                    detail.trim()
                ))
            }
        }
    }
}

fn apply_tmux_client_size(
    session: &str,
    client_tty: Option<&str>,
    cols: u16,
    rows: u16,
    fit_window: bool,
) -> Result<(), String> {
    if fit_window {
        return resize_session_window(session, cols, rows);
    }
    let Some(tty) = client_tty else {
        return Ok(());
    };
    match refresh_tmux_client(tty)? {
        TmuxClientRefresh::Refreshed | TmuxClientRefresh::ClientGone => Ok(()),
    }
}

pub(crate) struct TmuxOwnerGuard {
    registry: Arc<TmuxTerminalRegistry>,
    owner: Uuid,
}

impl TmuxOwnerGuard {
    pub(crate) fn new(registry: Arc<TmuxTerminalRegistry>, owner: Uuid) -> Self {
        Self { registry, owner }
    }

    pub(crate) fn owner(&self) -> Uuid {
        self.owner
    }
}

impl Drop for TmuxOwnerGuard {
    fn drop(&mut self) {
        self.registry.kill_owner(self.owner);
    }
}

pub(crate) fn clamp_terminal_cols(cols: u32) -> u16 {
    cols.clamp(TMUX_TERMINAL_COLS_MIN, TMUX_TERMINAL_COLS_MAX) as u16
}

pub(crate) fn clamp_terminal_rows(rows: u32) -> u16 {
    rows.clamp(TMUX_TERMINAL_ROWS_MIN, TMUX_TERMINAL_ROWS_MAX) as u16
}

fn decode_terminal_input(data: &str) -> Result<Vec<u8>, String> {
    if data.len() > TMUX_TERMINAL_INPUT_MAX {
        return Err("tmux terminal input exceeds 64 KiB".to_owned());
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|_| "tmux terminal input is not valid base64".to_owned())?;
    if bytes.len() > TMUX_TERMINAL_INPUT_MAX {
        return Err("tmux terminal input exceeds 64 KiB".to_owned());
    }
    Ok(bytes)
}

async fn coalesce_terminal_output(
    mut raw_rx: mpsc::Receiver<Vec<u8>>,
    events: mpsc::Sender<TmuxTerminalEvent>,
    terminal_id: String,
    exit_rx: oneshot::Receiver<i32>,
) {
    let mut buf = Vec::new();
    let mut flush = tokio::time::interval(TMUX_TERMINAL_FLUSH);
    flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            chunk = raw_rx.recv() => {
                match chunk {
                    Some(bytes) => {
                        buf.extend_from_slice(&bytes);
                        while buf.len() >= TMUX_TERMINAL_OUTPUT_RAW_MAX {
                            let frame: Vec<u8> = buf.drain(..TMUX_TERMINAL_OUTPUT_RAW_MAX).collect();
                            if !send_output_frame(&events, &terminal_id, &frame).await {
                                return;
                            }
                        }
                    }
                    None => {
                        if !buf.is_empty()
                            && !send_output_frame(&events, &terminal_id, &buf).await
                        {
                            return;
                        }
                        break;
                    }
                }
            }
            _ = flush.tick() => {
                if !buf.is_empty() {
                    let frame = std::mem::take(&mut buf);
                    if !send_output_frame(&events, &terminal_id, &frame).await {
                        return;
                    }
                }
            }
        }
    }
    let code = match tokio::time::timeout(TMUX_TIMEOUT, exit_rx).await {
        Ok(Ok(code)) => code,
        _ => -1,
    };
    let _ = tokio::time::timeout(
        TMUX_TIMEOUT,
        events.send(TmuxTerminalEvent::TmuxTerminalExit {
            terminal_id,
            code,
        }),
    )
    .await;
}

async fn send_output_frame(
    events: &mpsc::Sender<TmuxTerminalEvent>,
    terminal_id: &str,
    raw: &[u8],
) -> bool {
    let data = base64::engine::general_purpose::STANDARD.encode(raw);
    match tokio::time::timeout(
        TMUX_TIMEOUT,
        events.send(TmuxTerminalEvent::TmuxTerminalOutput {
            terminal_id: terminal_id.to_owned(),
            data,
        }),
    )
    .await
    {
        Ok(Ok(())) => true,
        Ok(Err(_)) | Err(_) => false,
    }
}

async fn enqueue_history_replay(
    events: &mpsc::Sender<TmuxTerminalEvent>,
    terminal_id: &str,
    pane_key: &str,
    history_key: &str,
    bytes: &[u8],
) {
    if bytes.is_empty() {
        if events
            .send(TmuxTerminalEvent::TmuxTerminalHistory {
                terminal_id: terminal_id.to_owned(),
                pane_key: pane_key.to_owned(),
                history_key: history_key.to_owned(),
                data: String::new(),
                is_final: true,
            })
            .await
            .is_err()
        {
            return;
        }
        return;
    }
    let mut offset = 0usize;
    while offset < bytes.len() {
        let end = (offset + TMUX_TERMINAL_PTY_CHUNK).min(bytes.len());
        let data = base64::engine::general_purpose::STANDARD.encode(&bytes[offset..end]);
        let is_final = end == bytes.len();
        // No per-chunk timeout: a phone on a slow link legitimately drains a
        // 2 MiB replay slower than any command timeout. The consumer closing
        // (detach, eviction, disconnect) is the real failure signal and
        // returns Err immediately; blocking here only backpressures the
        // replay, never the connection.
        if events
            .send(TmuxTerminalEvent::TmuxTerminalHistory {
                terminal_id: terminal_id.to_owned(),
                pane_key: pane_key.to_owned(),
                history_key: history_key.to_owned(),
                data,
                is_final,
            })
            .await
            .is_err()
        {
            return;
        }
        offset = end;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct StallWriter(Uuid);
    impl StallWriter {
        fn arm(owner: Uuid) -> Self {
            STALL_WRITER_OWNERS.lock().push(owner);
            Self(owner)
        }
    }
    impl Drop for StallWriter {
        fn drop(&mut self) {
            STALL_WRITER_OWNERS.lock().retain(|id| *id != self.0);
        }
    }

    static TEST_SESSION_SEQ: AtomicU64 = AtomicU64::new(0);

    fn tmux_binary_available() -> bool {
        Command::new("tmux")
            .arg("-V")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    #[test]
    fn pane_history_cache_reuses_bytes_and_purges_with_session() {
        let session = format!(
            "pi-hist-{}-{}",
            std::process::id(),
            TEST_SESSION_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let key = pane_history_key("1000", &session, "%9");
        purge_pane_history_for_session(&session);
        pane_history_store(key.clone(), session.clone(), Arc::new(b"ansi-bytes".to_vec()));
        assert_eq!(
            pane_history_lookup(&key).as_deref().map(Vec::as_slice),
            Some(b"ansi-bytes".as_slice())
        );
        purge_pane_history_for_session(&session);
        assert!(pane_history_lookup(&key).is_none());
    }

    #[test]
    fn parse_pane_meta_requires_daemon_epoch() {
        let parsed = parse_pane_meta(b"%3\t80\t24\t1710000000\n").expect("meta");
        assert_eq!(parsed.0, "%3");
        assert_eq!(parsed.1, 80);
        assert_eq!(parsed.2, 24);
        assert_eq!(parsed.3, "1710000000");
        assert!(parse_pane_meta(b"%3\t80\t24\n").is_err());
    }

    #[test]
    fn wait_std_child_bounded_reaps_killed_sleep() {
        let mut child = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("sleep");
        let _ = child.kill();
        let started = Instant::now();
        assert!(wait_std_child_bounded(&mut child, TMUX_TIMEOUT));
        assert!(started.elapsed() < TMUX_TIMEOUT);
    }

    #[test]
    fn wait_std_child_bounded_zero_timeout_does_not_block() {
        let mut child = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("sleep");
        let started = Instant::now();
        assert!(!wait_std_child_bounded(&mut child, Duration::ZERO));
        assert!(started.elapsed() < Duration::from_millis(200));
        let _ = child.kill();
        let _ = wait_std_child_bounded(&mut child, TMUX_TIMEOUT);
    }

    struct ThrowawaySession {
        name: String,
    }

    impl ThrowawaySession {
        fn create() -> Option<Self> {
            let name = format!(
                "pi-rpc-{}-{}",
                std::process::id(),
                TEST_SESSION_SEQ.fetch_add(1, Ordering::Relaxed)
            );
            Self::create_named(name)
        }

        fn create_named(name: String) -> Option<Self> {
            Self::create_named_argv(name, &["bash", "--norc", "--noprofile"])
        }

        fn create_with_argv(argv: &[&str]) -> Option<Self> {
            let name = format!(
                "pi-rpc-{}-{}",
                std::process::id(),
                TEST_SESSION_SEQ.fetch_add(1, Ordering::Relaxed)
            );
            Self::create_named_argv(name, argv)
        }

        fn create_named_argv(name: String, argv: &[&str]) -> Option<Self> {
            if !tmux_binary_available() {
                return None;
            }
            let selector = format!("={name}");
            let _ = Command::new("tmux")
                .args(["kill-session", "-t", &selector])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            let mut command = Command::new("tmux");
            command.args([
                "new-session",
                "-d",
                "-x",
                "200",
                "-y",
                "50",
                "-s",
                &name,
                "--",
            ]);
            command.args(argv);
            let status = command
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .expect("spawn tmux new-session for throwaway");
            assert!(
                status.success(),
                "tmux new-session failed for throwaway session {name}"
            );
            Some(Self { name })
        }
    }

    impl Drop for ThrowawaySession {
        fn drop(&mut self) {
            let selector = format!("={}", self.name);
            let _ = Command::new("tmux")
                .args(["kill-session", "-t", &selector])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }

    fn session_name_of_pane(pane: &str) -> String {
        let output = Command::new("tmux")
            .args(["display-message", "-t", pane, "-p", "#{session_name}"])
            .output()
            .expect("display-message session_name");
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout)
            .expect("utf8")
            .trim_end_matches(['\n', '\r'])
            .to_owned()
    }

    fn wait_for_pane_containing(session: &str, needle: &str) -> String {
        let deadline = Instant::now() + Duration::from_millis(10000);
        let mut last = String::new();
        loop {
            let snap = pane_snapshot(session, Some(80)).expect("snapshot");
            last = snap.content;
            if last.contains(needle) {
                return last;
            }
            if Instant::now() >= deadline {
                let tail: String = {
                    let total = last.chars().count();
                    last.chars()
                        .skip(total.saturating_sub(500))
                        .collect()
                };
                panic!(
                    "wait_for_pane_containing timed out needle={needle:?} session={session:?} content_tail={tail:?}"
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn parse_list_sessions_reads_spaces_attached_gt_one_and_windows() {
        let payload = b"agent with spaces\t3\t2\t1788958661\nsolo\t1\t0\t100\n";
        let sessions = parse_list_sessions(payload).expect("parse");
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].name, "agent with spaces");
        assert_eq!(sessions[0].windows, 3);
        assert!(sessions[0].attached);
        assert_eq!(sessions[0].created_epoch, 1788958661);
        assert_eq!(sessions[1].name, "solo");
        assert_eq!(sessions[1].windows, 1);
        assert!(!sessions[1].attached);
        assert_eq!(sessions[1].created_epoch, 100);
    }

    #[test]
    fn list_sessions_matches_live_tmux_names() {
        let Some(session) = ThrowawaySession::create() else {
            return;
        };
        let listed = list_sessions().expect("list");
        assert!(listed.tmux_available);
        let row = listed
            .sessions
            .iter()
            .find(|row| row.name == session.name)
            .unwrap_or_else(|| panic!("list missing {}", session.name));
        assert!(row.windows >= 1, "{row:?}");
        assert!(row.pane_count >= 1, "{row:?}");
        assert!(row.created_epoch > 0, "{row:?}");
        let name = session.name.clone();
        drop(session);
        let after = list_sessions().expect("list after kill");
        assert!(
            after.sessions.iter().all(|row| row.name != name),
            "{after:?}"
        );
    }

    #[test]
    fn pane_snapshot_captures_sentinel_and_redacts_sk_secret() {
        let Some(session) = ThrowawaySession::create() else {
            return;
        };
        send_keys(&session.name, Some("echo SENTINEL-PANE"), None).expect("text");
        send_keys(&session.name, None, Some("enter")).expect("enter");
        let content = wait_for_pane_containing(&session.name, "SENTINEL-PANE");
        let snap = pane_snapshot(&session.name, Some(80)).expect("snapshot");
        assert_eq!(snap.session, session.name);
        assert!(snap.pane.starts_with('%'), "{}", snap.pane);
        assert!(snap.width > 0 && snap.height > 0);
        assert!(content.contains("SENTINEL-PANE"), "{content}");
        send_keys(&session.name, Some("echo sk-testsecret12345678"), None).expect("secret text");
        send_keys(&session.name, None, Some("enter")).expect("enter");
        let redacted = wait_for_pane_containing(&session.name, "[REDACTED]");
        assert!(!redacted.contains("sk-"), "{redacted}");
        assert!(
            redacted.contains("[REDACTED]") || redacted.contains("SENTINEL-PANE"),
            "{redacted}"
        );
    }

    #[test]
    fn pane_snapshot_unknown_session_names_the_session() {
        if !tmux_binary_available() {
            return;
        }
        let name = format!("pi-rpc-missing-{}", std::process::id());
        let error = pane_snapshot(&name, None).expect_err("missing");
        assert!(
            error.contains(&format!("tmux session not found: {name}")),
            "{error}"
        );
    }

    #[test]
    fn invalid_session_names_rejected_without_tmux() {
        let empty = pane_snapshot("", None).expect_err("empty");
        assert!(empty.contains("empty"), "{empty}");
        let space = pane_snapshot("has space", None).expect_err("space");
        assert!(
            space.contains("whitespace") || space.contains("control"),
            "{space}"
        );
        let control = pane_snapshot("bad\nname", None).expect_err("control");
        assert!(
            control.contains("whitespace") || control.contains("control"),
            "{control}"
        );
        let dash = pane_snapshot("-hidden", None).expect_err("dash");
        assert!(dash.contains("'-'") || dash.contains("start"), "{dash}");
        for name in ["", " ", "bad\nname", "-hidden"] {
            let send = send_keys(name, Some("hi"), None).expect_err(name);
            assert!(
                send.contains("empty")
                    || send.contains("whitespace")
                    || send.contains("control")
                    || send.contains("'-'")
                    || send.contains("start"),
                "{name}: {send}"
            );
            assert!(
                !send.contains("tmux is not available"),
                "{name}: {send}"
            );
            assert!(
                !send.contains("session not found"),
                "{name}: {send}"
            );
        }
    }

    #[test]
    fn paste_text_rejects_invalid_names_without_tmux() {
        let empty = paste_text("", "/tmp/x").expect_err("empty");
        assert!(empty.contains("empty"), "{empty}");
        let dash = paste_text("-hidden", "/tmp/x").expect_err("dash");
        assert!(dash.contains("'-'") || dash.contains("start"), "{dash}");
        let space = paste_text("has space", "/tmp/x").expect_err("space");
        assert!(
            space.contains("whitespace") || space.contains("control"),
            "{space}"
        );
    }

    #[test]
    fn paste_cleanup_errors_join_the_primary() {
        assert_eq!(merge_cleanup("load failed".to_owned(), Ok(())), "load failed");
        assert_eq!(
            merge_cleanup(
                "paste-buffer failed".to_owned(),
                Err("tmux delete-buffer failed: gone".to_owned())
            ),
            "paste-buffer failed; tmux delete-buffer failed: gone"
        );
    }

    #[test]
    fn paste_buffer_file_missing_is_success() {
        let path = std::env::temp_dir().join(format!("rpi-tmux-paste-absent-{}", Uuid::now_v7()));
        remove_paste_buffer_file(&path).expect("missing");
    }

    #[test]
    fn paste_buffer_file_directory_propagates() {
        let path = std::env::temp_dir().join(format!("rpi-tmux-paste-dir-{}", Uuid::now_v7()));
        std::fs::create_dir(&path).expect("dir");
        let error = remove_paste_buffer_file(&path).expect_err("directory");
        assert!(error.contains("remove tmux paste buffer failed"), "{error}");
        std::fs::remove_dir(&path).expect("cleanup");
    }

    #[test]
    fn paste_text_inserts_the_path_into_the_session() {
        let Some(session) = ThrowawaySession::create() else {
            return;
        };
        paste_text(&session.name, "RPI-TMUX-PATH").expect("paste");
        let deadline = Instant::now() + Duration::from_millis(10000);
        let mut last = String::new();
        loop {
            let snap = pane_snapshot(&session.name, Some(80)).expect("snapshot");
            last = snap.content;
            let compact: String = last.chars().filter(|ch| *ch != '\n' && *ch != '\r').collect();
            if compact.contains("RPI-TMUX-PATH") {
                return;
            }
            if Instant::now() >= deadline {
                panic!(
                    "paste did not appear session={:?} content={last:?}",
                    session.name
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn create_and_delete_session_round_trip() {
        if !tmux_binary_available() {
            return;
        }
        let name = format!(
            "pi-rpc-create-{}-{}",
            std::process::id(),
            TEST_SESSION_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let created = create_session(&name).expect("create");
        assert!(created.created);
        assert_eq!(created.session, name);
        let listed = list_sessions().expect("list");
        assert!(listed.sessions.iter().any(|row| row.name == name));
        let duplicate = create_session(&name).expect_err("duplicate");
        assert!(
            duplicate.contains("already exists"),
            "{duplicate}"
        );
        let deleted = delete_session(&name).expect("delete");
        assert!(deleted.deleted);
        assert_eq!(deleted.session, name);
        let after = list_sessions().expect("list after");
        assert!(after.sessions.iter().all(|row| row.name != name));
        let missing = delete_session(&name).expect_err("missing");
        assert!(
            missing.contains(&format!("tmux session not found: {name}")),
            "{missing}"
        );
    }

    #[test]
    fn create_and_delete_reject_invalid_names_without_tmux() {
        for name in ["", " ", "bad\nname", "-hidden"] {
            let create = create_session(name).expect_err(name);
            let delete = delete_session(name).expect_err(name);
            for error in [create, delete] {
                assert!(
                    error.contains("empty")
                        || error.contains("whitespace")
                        || error.contains("control")
                        || error.contains("'-'")
                        || error.contains("start"),
                    "{name}: {error}"
                );
                assert!(!error.contains("tmux is not available"), "{name}: {error}");
                assert!(!error.contains("session not found"), "{name}: {error}");
            }
        }
    }

    #[test]
    fn capture_lines_clamp_to_500() {
        assert_eq!(clamp_capture_lines(None), 80);
        assert_eq!(clamp_capture_lines(Some(0)), 1);
        assert_eq!(clamp_capture_lines(Some(80)), 80);
        assert_eq!(clamp_capture_lines(Some(500)), 500);
        assert_eq!(clamp_capture_lines(Some(501)), 500);
        assert_eq!(clamp_capture_lines(Some(u32::MAX)), 500);
    }

    #[test]
    fn terminal_history_capture_is_one_bounded_start() {
        assert_eq!(TMUX_TERMINAL_CAPTURE_START, "-10000");
        assert_eq!(TMUX_TERMINAL_CAPTURE_STDOUT_CAP, 2 * 1024 * 1024);
        assert_eq!(
            terminal_history_capture_args("%3"),
            [
                "capture-pane",
                "-e",
                "-p",
                "-t",
                "%3",
                "-S",
                "-10000",
                "-E",
                "-1",
            ]
        );
        assert!(
            !terminal_history_capture_args("%3")
                .iter()
                .any(|arg| *arg == "-400" || *arg == "-"),
            "capture must not keep a second start bound"
        );
    }

    #[tokio::test]
    async fn history_replay_completes_when_consumer_drains_a_full_queue() {
        let (tx, mut rx) = mpsc::channel(1);
        let bytes = vec![b'a'; TMUX_TERMINAL_PTY_CHUNK * 3];
        let replay = tokio::spawn(async move {
            enqueue_history_replay(&tx, "tid", "%1", "hk", &bytes).await;
        });
        let mut chunks = 0usize;
        let mut saw_final = false;
        while let Some(event) = rx.recv().await {
            let TmuxTerminalEvent::TmuxTerminalHistory { is_final, .. } = event else {
                panic!("expected history event");
            };
            chunks += 1;
            if is_final {
                saw_final = true;
                break;
            }
        }
        replay.await.expect("replay task");
        assert!(saw_final);
        assert!(chunks >= 3, "chunks={chunks}");
    }

    #[tokio::test]
    async fn history_replay_waits_past_command_timeout_for_a_slow_consumer() {
        let (tx, mut rx) = mpsc::channel(1);
        let bytes = vec![b'a'; TMUX_TERMINAL_PTY_CHUNK + 1];
        let replay = tokio::spawn(async move {
            enqueue_history_replay(&tx, "tid", "%1", "hk", &bytes).await;
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !replay.is_finished(),
            "second chunk must block on the full queue"
        );
        tokio::time::sleep(TMUX_TIMEOUT + Duration::from_millis(200)).await;
        assert!(
            !replay.is_finished(),
            "replay must stay blocked on the full queue after TMUX_TIMEOUT"
        );
        let mut saw_final = false;
        while let Some(event) = rx.recv().await {
            let TmuxTerminalEvent::TmuxTerminalHistory { is_final, .. } = event else {
                panic!("expected history");
            };
            if is_final {
                saw_final = true;
                break;
            }
        }
        replay.await.expect("replay task");
        assert!(saw_final);
    }

    #[tokio::test]
    async fn history_replay_ends_when_consumer_closes() {
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        let bytes = vec![b'a'; TMUX_TERMINAL_PTY_CHUNK * 4];
        tokio::time::timeout(
            Duration::from_secs(1),
            enqueue_history_replay(&tx, "tid", "%1", "hk", &bytes),
        )
        .await
        .expect("closed consumer must finish replay without hanging");
    }

    #[test]
    fn send_keys_text_and_enter_appear_in_throwaway_pane() {
        let Some(session) = ThrowawaySession::create() else {
            return;
        };
        send_keys(&session.name, Some("echo SENTINEL-SEND"), None).expect("text");
        send_keys(&session.name, None, Some("enter")).expect("enter");
        let content = wait_for_pane_containing(&session.name, "SENTINEL-SEND");
        assert!(content.contains("SENTINEL-SEND"), "{content}");
    }

    #[test]
    fn send_keys_literal_c_c_does_not_interpret_as_interrupt() {
        let Some(session) = ThrowawaySession::create() else {
            return;
        };
        send_keys(&session.name, Some("C-c"), None).expect("literal");
        let content = wait_for_pane_containing(&session.name, "C-c");
        assert!(content.contains("C-c"), "{content}");
        assert!(!content.contains("^C"), "{content}");
    }

    #[test]
    fn send_keys_rejects_both_neither_unknown_key_and_unknown_session() {
        let neither = send_keys("ok-session", None, None).expect_err("neither");
        assert!(neither.contains("exactly one of text or key"), "{neither}");
        let both = send_keys("ok-session", Some("hi"), Some("enter")).expect_err("both");
        assert!(both.contains("exactly one of text or key"), "{both}");
        let bad_key = send_keys("ok-session", None, Some("ctrl_x")).expect_err("enum");
        assert!(bad_key.contains("enter, escape, ctrl_c"), "{bad_key}");
        let empty_text = send_keys("ok-session", Some(""), None).expect_err("empty text");
        assert!(empty_text.contains("non-empty"), "{empty_text}");
        if tmux_binary_available() {
            let name = format!("pi-rpc-missing-send-{}", std::process::id());
            let error = send_keys(&name, Some("hi"), None).expect_err("missing");
            assert!(
                error.contains(&format!("tmux session not found: {name}")),
                "{error}"
            );
        }
    }

    #[test]
    fn exact_session_name_rejects_prefix_and_targets_shorter_sibling() {
        let pid = std::process::id();
        let short_name = format!("pi-x-{pid}");
        let long_name = format!("pi-x-{pid}-longer");
        let Some(short) = ThrowawaySession::create_named(short_name) else {
            return;
        };
        let Some(long) = ThrowawaySession::create_named(long_name) else {
            return;
        };
        send_keys(&short.name, Some("echo SHORT-PANE"), None).expect("short text");
        send_keys(&short.name, None, Some("enter")).expect("short enter");
        send_keys(&long.name, Some("echo LONG-PANE"), None).expect("long text");
        send_keys(&long.name, None, Some("enter")).expect("long enter");
        let short_content = wait_for_pane_containing(&short.name, "SHORT-PANE");
        let long_content = wait_for_pane_containing(&long.name, "LONG-PANE");
        assert!(short_content.contains("SHORT-PANE"), "{short_content}");
        assert!(long_content.contains("LONG-PANE"), "{long_content}");
        let snap = pane_snapshot(&short.name, Some(80)).expect("short snap");
        assert_eq!(snap.session, short.name);
        assert_eq!(session_name_of_pane(&snap.pane), short.name);
        let long_snap = pane_snapshot(&long.name, Some(80)).expect("long snap");
        assert_eq!(session_name_of_pane(&long_snap.pane), long.name);
        let sent = send_keys(&short.name, Some("echo SHORT-SEND"), None).expect("short send");
        assert_eq!(sent.session, short.name);
        assert!(sent.sent);
        assert_eq!(session_name_of_pane(&sent.pane), short.name);
        send_keys(&short.name, None, Some("enter")).expect("short send enter");
        let short_after = wait_for_pane_containing(&short.name, "SHORT-SEND");
        assert!(short_after.contains("SHORT-SEND"), "{short_after}");
        let long_after = pane_snapshot(&long.name, Some(80)).expect("long after send");
        assert!(
            !long_after.content.contains("SHORT-SEND"),
            "{}",
            long_after.content
        );
        let prefix = "pi-x";
        let snap_err = pane_snapshot(prefix, None).expect_err("prefix snap");
        assert!(
            snap_err.contains(&format!("tmux session not found: {prefix}")),
            "{snap_err}"
        );
        let send_err = send_keys(prefix, Some("hi"), None).expect_err("prefix send");
        assert!(
            send_err.contains(&format!("tmux session not found: {prefix}")),
            "{send_err}"
        );
    }

    fn session_attached_count(session: &str) -> u32 {
        require_session(session).unwrap_or_else(|error| panic!("{error}"));
        let selector = exact_session_selector(session);
        let output = match run_tmux(
            &["list-clients", "-t", &selector, "-F", "#{client_name}"],
            TMUX_LIST_STDOUT_CAP,
        ) {
            Ok(output) => output,
            Err(error) => panic!("{}", format_run_error("list-clients", error)),
        };
        if output.code != 0 {
            panic!(
                "list-clients failed code={} stdout={:?}",
                output.code,
                String::from_utf8_lossy(&output.stdout)
            );
        }
        String::from_utf8_lossy(&output.stdout)
            .split('\n')
            .filter(|line| !line.is_empty())
            .count() as u32
    }

    fn session_client_pids(session: &str) -> Vec<u32> {
        require_session(session).unwrap_or_else(|error| panic!("{error}"));
        let selector = exact_session_selector(session);
        let output = match run_tmux(
            &["list-clients", "-t", &selector, "-F", "#{client_pid}"],
            TMUX_LIST_STDOUT_CAP,
        ) {
            Ok(output) => output,
            Err(error) => panic!("{}", format_run_error("list-clients", error)),
        };
        if output.code != 0 {
            panic!(
                "list-clients failed code={} stdout={:?}",
                output.code,
                String::from_utf8_lossy(&output.stdout)
            );
        }
        String::from_utf8_lossy(&output.stdout)
            .split('\n')
            .filter(|line| !line.is_empty())
            .map(|line| {
                line.trim()
                    .parse::<u32>()
                    .unwrap_or_else(|_| panic!("client_pid {line:?}"))
            })
            .collect()
    }

    fn process_state(pid: u32) -> Option<char> {
        let output = Command::new("ps")
            .args(["-o", "state=", "-p", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        String::from_utf8_lossy(&output.stdout)
            .trim()
            .chars()
            .next()
    }

    fn process_is_alive(pid: u32) -> bool {
        matches!(process_state(pid), Some(state) if state != 'Z')
    }

    fn process_is_zombie(pid: u32) -> bool {
        matches!(process_state(pid), Some('Z'))
    }

    fn spawned_child_pid(owner: Uuid) -> Option<u32> {
        LAST_SPAWNED_CHILD
            .lock()
            .iter()
            .find(|(row_owner, _)| *row_owner == owner)
            .map(|(_, pid)| *pid)
    }

    fn try_session_attached_count(session: &str) -> Option<u32> {
        let selector = exact_session_selector(session);
        let output = run_tmux(
            &["list-clients", "-t", &selector, "-F", "#{client_name}"],
            TMUX_LIST_STDOUT_CAP,
        )
        .ok()?;
        if output.code != 0 {
            return None;
        }
        Some(
            String::from_utf8_lossy(&output.stdout)
                .split('\n')
                .filter(|line| !line.is_empty())
                .count() as u32,
        )
    }

    async fn wait_until(label: &str, mut ready: impl FnMut() -> Result<(), String>) {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(10000);
        let mut last = String::new();
        loop {
            match ready() {
                Ok(()) => return,
                Err(status) => last = status,
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("{label} timed out last={last}");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn session_window_size(session: &str) -> String {
        let target = exact_active_target(session);
        let output = match run_tmux(
            &["show-options", "-t", &target, "-v", "window-size"],
            TMUX_LIST_STDOUT_CAP,
        ) {
            Ok(output) => output,
            Err(error) => panic!("{}", format_run_error("show-options", error)),
        };
        assert_eq!(output.code, 0, "window-size option failed");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn pane_winsize(session: &str) -> (u32, u32) {
        let target = exact_active_target(session);
        let output = match run_tmux(
            &[
                "display-message",
                "-t",
                &target,
                "-p",
                "#{pane_width}\t#{pane_height}",
            ],
            TMUX_LIST_STDOUT_CAP,
        ) {
            Ok(output) => output,
            Err(error) => panic!("{}", format_run_error("display-message", error)),
        };
        assert_eq!(output.code, 0, "pane winsize failed");
        let text = String::from_utf8_lossy(&output.stdout);
        let mut fields = text.trim().split('\t');
        let width = fields.next().expect("width").parse().expect("width");
        let height = fields.next().expect("height").parse().expect("height");
        (width, height)
    }

    fn pane_pid(session: &str) -> u32 {
        let target = exact_active_target(session);
        let output = match run_tmux(
            &["display-message", "-t", &target, "-p", "#{pane_pid}"],
            TMUX_LIST_STDOUT_CAP,
        ) {
            Ok(output) => output,
            Err(error) => panic!("{}", format_run_error("display-message", error)),
        };
        assert_eq!(output.code, 0, "pane pid failed");
        String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .expect("pane pid")
    }

    fn pane_is_dead(session: &str) -> bool {
        let target = exact_active_target(session);
        let output = match run_tmux(
            &["display-message", "-t", &target, "-p", "#{pane_dead}"],
            TMUX_LIST_STDOUT_CAP,
        ) {
            Ok(output) => output,
            Err(error) => panic!("{}", format_run_error("display-message", error)),
        };
        assert_eq!(output.code, 0, "pane dead failed");
        String::from_utf8_lossy(&output.stdout).trim() == "1"
    }

    async fn collect_output_until(
        events: &mut mpsc::Receiver<TmuxTerminalEvent>,
        needle: &str,
    ) -> String {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let mut decoded = String::new();
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                panic!("timed out waiting for {needle:?} in {decoded:?}");
            }
            match tokio::time::timeout(remaining, events.recv()).await {
                Ok(Some(TmuxTerminalEvent::TmuxTerminalOutput { data, .. })) => {
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(&data)
                        .expect("output base64");
                    decoded.push_str(&String::from_utf8_lossy(&bytes));
                    if decoded.contains(needle) {
                        return decoded;
                    }
                }
                Ok(Some(TmuxTerminalEvent::TmuxTerminalHistory { .. })) => {}
                Ok(Some(TmuxTerminalEvent::TmuxTerminalExit { code, .. })) => {
                    panic!("terminal exited {code} before {needle:?}: {decoded:?}");
                }
                Ok(None) => panic!("event channel closed before {needle:?}: {decoded:?}"),
                Err(_) => panic!("timed out waiting for {needle:?} in {decoded:?}"),
            }
        }
    }

    #[tokio::test]
    async fn tmux_terminal_attach_input_round_trip_sentinel() {
        let Some(session) = ThrowawaySession::create() else {
            return;
        };
        let registry = TmuxTerminalRegistry::new();
        let owner = Uuid::now_v7();
        let (attached, mut events) = registry
            .attach(owner, &session.name, 80, 24, false, None)
            .await
            .expect("attach");
        assert_eq!(attached.session, session.name);
        assert_eq!(attached.cols, 80);
        assert_eq!(attached.rows, 24);
        let payload = base64::engine::general_purpose::STANDARD.encode("printf SENTINEL\r");
        registry
            .input(owner, &attached.terminal_id, &payload)
            .await
            .expect("input");
        let output = collect_output_until(&mut events, "SENTINEL").await;
        assert!(output.contains("SENTINEL"), "{output}");
        registry
            .detach(owner, &attached.terminal_id)
            .expect("detach");
    }

    #[tokio::test]
    async fn tmux_terminal_da_reply_on_attach_pty_reaches_pane_stdin() {
        // bash readline swallows an unrecognized CSI, so a DA-shaped reply
        // never shows up in the pane echo. `cat` writes every byte back.
        let Some(session) = ThrowawaySession::create_with_argv(&[
            "bash",
            "--norc",
            "--noprofile",
            "-c",
            "exec cat",
        ]) else {
            return;
        };
        let registry = TmuxTerminalRegistry::new();
        let owner = Uuid::now_v7();
        let (attached, mut events) = registry
            .attach(owner, &session.name, 80, 24, false, None)
            .await
            .expect("attach");
        let da_reply = b"\x1b[>84;0;0c";
        let mut typed = da_reply.to_vec();
        typed.extend_from_slice(b"printf PANE-GOT\r");
        let payload = base64::engine::general_purpose::STANDARD.encode(&typed);
        registry
            .input(owner, &attached.terminal_id, &payload)
            .await
            .expect("input");
        let output = collect_output_until(&mut events, "PANE-GOT").await;
        assert!(output.contains("PANE-GOT"), "{output}");
        assert!(
            output.contains("84;0;0") || output.contains("\u{1b}[>84;0;0c"),
            "DA reply written to the attach PTY must appear as pane keystroke echo: {output:?}"
        );
        registry
            .detach(owner, &attached.terminal_id)
            .expect("detach");
    }

    #[tokio::test]
    async fn tmux_terminal_pane_da_query_is_forwarded_to_attach_pty() {
        let Some(session) = ThrowawaySession::create_with_argv(&[
            "bash",
            "--norc",
            "--noprofile",
            "-c",
            "printf '\\033[>c'; exec cat",
        ]) else {
            return;
        };
        let registry = TmuxTerminalRegistry::new();
        let owner = Uuid::now_v7();
        let (attached, mut events) = registry
            .attach(owner, &session.name, 80, 24, false, None)
            .await
            .expect("attach");
        let output = collect_output_until(&mut events, "\u{1b}[>c").await;
        assert!(
            output.contains("\u{1b}[>c"),
            "pane DA query must reach the attach PTY (outer terminal): {output:?}"
        );
        registry
            .detach(owner, &attached.terminal_id)
            .expect("detach");
    }

    #[tokio::test]
    async fn delete_session_refuses_active_terminals() {
        let Some(session) = ThrowawaySession::create() else {
            return;
        };
        let registry = TmuxTerminalRegistry::new();
        let owner = Uuid::now_v7();
        let (attached, _events) = registry
            .attach(owner, &session.name, 80, 24, false, None)
            .await
            .expect("attach");
        let error = delete_session(&session.name).expect_err("active");
        assert!(
            error.contains("tmux session has active terminals"),
            "{error}"
        );
        registry
            .detach(owner, &attached.terminal_id)
            .expect("detach");
        let deleted = delete_session(&session.name).expect("delete after detach");
        assert!(deleted.deleted);
        assert_eq!(deleted.session, session.name);
        std::mem::forget(session);
    }

    #[tokio::test]
    async fn tmux_terminal_unknown_session_fails_typed_without_spawn() {
        let registry = TmuxTerminalRegistry::new();
        let owner = Uuid::now_v7();
        let error = registry
            .attach(owner, "pi-missing-terminal-session", 80, 24, false, None)
            .await
            .err()
            .expect("unknown session");
        assert!(
            error.contains("tmux session not found: pi-missing-terminal-session"),
            "{error}"
        );
        assert_eq!(registry.live_count(), 0);
    }

    #[tokio::test]
    async fn tmux_terminal_invalid_name_rejected_before_spawn() {
        let registry = TmuxTerminalRegistry::new();
        let owner = Uuid::now_v7();
        let empty = registry.attach(owner, "", 80, 24, false, None).await.err().expect("empty");
        assert!(empty.contains("tmux session name is empty"), "{empty}");
        let dashed = registry
            .attach(owner, "-bad", 80, 24, false, None)
            .await
            .err()
            .expect("leading dash");
        assert!(
            dashed.contains("tmux session name must not start with '-'"),
            "{dashed}"
        );
        let space = registry
            .attach(owner, "bad name", 80, 24, false, None)
            .await
            .err()
            .expect("whitespace");
        assert!(
            space.contains("whitespace or control characters"),
            "{space}"
        );
        assert_eq!(registry.live_count(), 0);
    }

    #[tokio::test]
    async fn tmux_terminal_attach_sets_manual_window_size() {
        let Some(session) = ThrowawaySession::create() else {
            return;
        };
        let registry = TmuxTerminalRegistry::new();
        let owner = Uuid::now_v7();
        let (attached, _events) = registry
            .attach(owner, &session.name, 45, 20, false, None)
            .await
            .expect("attach");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let (width, height) = pane_winsize(&session.name);
            if width == 45 && height == 20 && session_window_size(&session.name) == "manual" {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!(
                    "attach stayed {width}x{height} window-size={}",
                    session_window_size(&session.name)
                );
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        registry
            .detach(owner, &attached.terminal_id)
            .expect("detach");
    }

    #[tokio::test]
    async fn tmux_terminal_attach_returns_before_history_replay_finishes() {
        let Some(session) = ThrowawaySession::create() else {
            return;
        };
        let registry = TmuxTerminalRegistry::new();
        let owner = Uuid::now_v7();
        let attached = registry
            .attach(owner, &session.name, 80, 24, true, None)
            .await;
        let (attached, mut events) = match attached {
            Ok(pair) => pair,
            Err(error) => {
                assert!(
                    !error.contains("tmux terminal history replay failed"),
                    "{error}"
                );
                panic!("attach failed: {error}");
            }
        };
        let mut saw_history = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(200), events.recv()).await {
                Ok(Some(TmuxTerminalEvent::TmuxTerminalHistory { is_final, .. })) => {
                    saw_history = true;
                    if is_final {
                        break;
                    }
                }
                Ok(Some(TmuxTerminalEvent::TmuxTerminalOutput { .. })) => {}
                Ok(Some(TmuxTerminalEvent::TmuxTerminalExit { .. })) => break,
                Ok(None) => break,
                Err(_) => {
                    if saw_history {
                        break;
                    }
                }
            }
        }
        let _ = registry.detach(owner, &attached.terminal_id);
        assert!(saw_history, "history must stream after attach returns");
    }

    #[tokio::test]
    async fn tmux_terminal_resize_updates_pane_winsize() {
        let Some(session) = ThrowawaySession::create() else {
            return;
        };
        let registry = TmuxTerminalRegistry::new();
        let owner = Uuid::now_v7();
        let (attached, _events) = registry
            .attach(owner, &session.name, 80, 24, false, None)
            .await
            .expect("attach");
        registry
            .resize(owner, &attached.terminal_id, 100, 30, true)
            .await
            .expect("resize");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let (width, height) = pane_winsize(&session.name);
            if width == 100 && height == 30 {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("pane winsize stayed {width}x{height}");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(session_window_size(&session.name), "manual");
        registry
            .detach(owner, &attached.terminal_id)
            .expect("detach");
        let restored = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            if session_window_size(&session.name) == "largest" {
                break;
            }
            if tokio::time::Instant::now() >= restored {
                panic!(
                    "detach left window-size {}",
                    session_window_size(&session.name)
                );
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    #[test]
    fn parse_refresh_client_gone_rejects_other_tmux_errors() {
        assert!(parse_refresh_client_gone("can't find client: /dev/pts/4"));
        assert!(!parse_refresh_client_gone("not a control client"));
        assert!(!parse_refresh_client_gone("tmux refresh-client timed out"));
    }

    #[test]
    fn apply_tmux_client_size_liveness_without_tty_is_ok() {
        apply_tmux_client_size("pi-unused", None, 80, 24, false)
            .expect("liveness resize does not require a client tty");
    }

    #[test]
    fn apply_tmux_client_size_fit_without_tty_resizes_window() {
        let Some(session) = ThrowawaySession::create() else {
            return;
        };
        apply_tmux_client_size(&session.name, None, 45, 20, true).expect("fit");
        assert_eq!(session_window_size(&session.name), "manual");
        assert_eq!(pane_winsize(&session.name), (45, 20));
    }

    #[tokio::test]
    async fn tmux_terminal_liveness_resize_does_not_force_window() {
        let Some(session) = ThrowawaySession::create() else {
            return;
        };
        let registry = TmuxTerminalRegistry::new();
        let owner = Uuid::now_v7();
        let (attached, _events) = registry
            .attach(owner, &session.name, 80, 24, false, None)
            .await
            .expect("attach");
        let sized = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            if pane_winsize(&session.name) == (80, 24) && session_window_size(&session.name) == "manual"
            {
                break;
            }
            if tokio::time::Instant::now() >= sized {
                panic!(
                    "attach stayed {:?} window-size={}",
                    pane_winsize(&session.name),
                    session_window_size(&session.name)
                );
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        registry
            .resize(owner, &attached.terminal_id, 100, 30, false)
            .await
            .expect("liveness resize");
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(pane_winsize(&session.name), (80, 24));
        assert_eq!(session_window_size(&session.name), "manual");
        registry
            .detach(owner, &attached.terminal_id)
            .expect("detach");
    }

    #[tokio::test]
    async fn tmux_terminal_fit_keeps_attach_client_and_pane() {
        let Some(session) = ThrowawaySession::create_with_argv(&[
            "bash",
            "--norc",
            "--noprofile",
            "-c",
            "printf READY; exec cat",
        ]) else {
            return;
        };
        let pane = pane_pid(&session.name);
        let native = spawn_session_client(&session.name, 120, 40);
        wait_attached_count(&session.name, 1).await;
        let registry = TmuxTerminalRegistry::new();
        let owner = Uuid::now_v7();
        let (attached, mut events) = registry
            .attach(owner, &session.name, 45, 20, false, None)
            .await
            .expect("attach");
        wait_attached_count(&session.name, 2).await;
        let child = spawned_child_pid(owner).expect("attach child");
        registry
            .resize(owner, &attached.terminal_id, 45, 20, true)
            .await
            .expect("fit");
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert!(process_is_alive(pane), "pane pid {pane} died after Fit");
        assert!(
            process_is_alive(child),
            "attach client {child} died after Fit"
        );
        assert!(!pane_is_dead(&session.name), "pane marked dead after Fit");
        loop {
            match tokio::time::timeout(Duration::from_millis(50), events.recv()).await {
                Ok(Some(TmuxTerminalEvent::TmuxTerminalExit { code, .. })) => {
                    panic!("attach PTY exited {code} after Fit");
                }
                Ok(Some(TmuxTerminalEvent::TmuxTerminalOutput { data, .. })) => {
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(&data)
                        .expect("output base64");
                    let text = String::from_utf8_lossy(&bytes).to_ascii_lowercase();
                    assert!(
                        !text.contains("lost tty"),
                        "attach output contained lost tty: {text}"
                    );
                }
                Ok(Some(TmuxTerminalEvent::TmuxTerminalHistory { .. })) => {}
                Ok(None) => panic!("event channel closed after Fit"),
                Err(_) => break,
            }
        }
        registry
            .detach(owner, &attached.terminal_id)
            .expect("detach");
        drop(native);
    }

    #[tokio::test]
    async fn tmux_terminal_detach_restores_largest_for_remaining_client() {
        let Some(session) = ThrowawaySession::create() else {
            return;
        };
        let registry = TmuxTerminalRegistry::new();
        let desktop = Uuid::now_v7();
        let phone = Uuid::now_v7();
        let (desktop_attached, _desktop_events) = registry
            .attach(desktop, &session.name, 120, 40, false, None)
            .await
            .expect("desktop attach");
        let (phone_attached, _phone_events) = registry
            .attach(phone, &session.name, 45, 20, false, None)
            .await
            .expect("phone attach");
        registry
            .resize(phone, &phone_attached.terminal_id, 45, 20, true)
            .await
            .expect("phone fit");
        let fitted = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let (width, height) = pane_winsize(&session.name);
            if width == 45 && height == 20 && session_window_size(&session.name) == "manual" {
                break;
            }
            if tokio::time::Instant::now() >= fitted {
                panic!(
                    "phone fit stayed {width}x{height} window-size={}",
                    session_window_size(&session.name)
                );
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        registry
            .detach(phone, &phone_attached.terminal_id)
            .expect("phone detach");
        let restored = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let (width, height) = pane_winsize(&session.name);
            if width == 120 && height == 40 && session_window_size(&session.name) == "largest" {
                break;
            }
            if tokio::time::Instant::now() >= restored {
                panic!(
                    "desktop stayed {width}x{height} window-size={} after phone detach",
                    session_window_size(&session.name)
                );
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        registry
            .detach(desktop, &desktop_attached.terminal_id)
            .expect("desktop detach");
    }

    struct NativeSessionClient {
        _master: Box<dyn MasterPty + Send>,
        killer: Box<dyn portable_pty::ChildKiller + Send + Sync>,
        _child: Box<dyn portable_pty::Child + Send + Sync>,
    }

    impl Drop for NativeSessionClient {
        fn drop(&mut self) {
            let _ = self.killer.kill();
        }
    }

    fn spawn_session_client(session: &str, cols: u16, rows: u16) -> NativeSessionClient {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("native pty");
        let child = pair
            .slave
            .spawn_command(attach_session_command(session))
            .expect("native attach");
        let killer = child.clone_killer();
        drop(pair.slave);
        NativeSessionClient {
            _master: pair.master,
            killer,
            _child: child,
        }
    }

    async fn wait_attached_count(session: &str, want: u32) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            if session_attached_count(session) >= want {
                return;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!(
                    "attached count stayed {} want {want}",
                    session_attached_count(session)
                );
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    async fn tmux_terminal_detach_restores_largest_with_native_client() {
        let Some(session) = ThrowawaySession::create() else {
            return;
        };
        let native = spawn_session_client(&session.name, 80, 24);
        wait_attached_count(&session.name, 1).await;
        let registry = TmuxTerminalRegistry::new();
        let desktop = Uuid::now_v7();
        let phone = Uuid::now_v7();
        let (desktop_attached, _desktop_events) = registry
            .attach(desktop, &session.name, 120, 40, false, None)
            .await
            .expect("desktop attach");
        let (phone_attached, _phone_events) = registry
            .attach(phone, &session.name, 45, 20, false, None)
            .await
            .expect("phone attach");
        wait_attached_count(&session.name, 3).await;
        registry
            .resize(phone, &phone_attached.terminal_id, 45, 20, true)
            .await
            .expect("phone fit");
        let fitted = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let (width, height) = pane_winsize(&session.name);
            if width == 45 && height == 20 && session_window_size(&session.name) == "manual" {
                break;
            }
            if tokio::time::Instant::now() >= fitted {
                panic!(
                    "phone fit stayed {width}x{height} window-size={}",
                    session_window_size(&session.name)
                );
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        registry
            .detach(phone, &phone_attached.terminal_id)
            .expect("phone detach");
        let restored = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let (width, height) = pane_winsize(&session.name);
            if width == 120 && height == 40 && session_window_size(&session.name) == "largest" {
                break;
            }
            if tokio::time::Instant::now() >= restored {
                panic!(
                    "desktop+native stayed {width}x{height} window-size={} after phone detach",
                    session_window_size(&session.name)
                );
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        registry
            .detach(desktop, &desktop_attached.terminal_id)
            .expect("desktop detach");
        drop(native);
    }

    #[tokio::test]
    async fn tmux_terminal_inflight_fit_does_not_stick_after_detach() {
        let Some(session) = ThrowawaySession::create() else {
            return;
        };
        let registry = TmuxTerminalRegistry::new();
        let desktop = Uuid::now_v7();
        let phone = Uuid::now_v7();
        let (desktop_attached, _desktop_events) = registry
            .attach(desktop, &session.name, 120, 40, false, None)
            .await
            .expect("desktop attach");
        let (phone_attached, _phone_events) = registry
            .attach(phone, &session.name, 45, 20, false, None)
            .await
            .expect("phone attach");
        let phone_id = phone_attached.terminal_id.clone();
        let fit = registry.resize(phone, &phone_id, 45, 20, true);
        registry.detach(phone, &phone_id).expect("phone detach");
        let _ = fit.await;
        let restored = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let (width, height) = pane_winsize(&session.name);
            if width == 120 && height == 40 && session_window_size(&session.name) == "largest" {
                break;
            }
            if tokio::time::Instant::now() >= restored {
                panic!(
                    "in-flight fit stuck {width}x{height} window-size={}",
                    session_window_size(&session.name)
                );
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        registry
            .detach(desktop, &desktop_attached.terminal_id)
            .expect("desktop detach");
    }

    #[tokio::test]
    async fn tmux_terminal_detach_kills_client_and_keeps_session() {
        let Some(session) = ThrowawaySession::create() else {
            return;
        };
        let registry = TmuxTerminalRegistry::new();
        let owner = Uuid::now_v7();
        let (attached, _events) = registry
            .attach(owner, &session.name, 80, 24, false, None)
            .await
            .expect("attach");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            if session_attached_count(&session.name) >= 1 {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("attach did not increment session_attached");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        registry
            .detach(owner, &attached.terminal_id)
            .expect("detach");
        let gone = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            if session_attached_count(&session.name) == 0 && registry.live_count() == 0 {
                break;
            }
            if tokio::time::Instant::now() >= gone {
                panic!(
                    "client still attached count={} live={}",
                    session_attached_count(&session.name),
                    registry.live_count()
                );
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        require_session(&session.name).expect("session must survive detach");
    }

    #[tokio::test]
    async fn tmux_terminal_cap_rejects_seventeenth_attach() {
        let Some(session) = ThrowawaySession::create() else {
            return;
        };
        let registry = TmuxTerminalRegistry::new();
        let owner = Uuid::now_v7();
        let mut ids = Vec::new();
        let mut events = Vec::new();
        for _ in 0..TMUX_TERMINAL_MAX {
            let (attached, rx) = registry
                .attach(owner, &session.name, 80, 24, false, None)
                .await
                .expect("attach within cap");
            ids.push(attached.terminal_id);
            events.push(rx);
        }
        assert_eq!(events.len(), TMUX_TERMINAL_MAX);
        assert_eq!(
            registry.live_count(),
            TMUX_TERMINAL_MAX,
            "cap slots must stay occupied before the 17th attach"
        );
        let error = registry
            .attach(owner, &session.name, 80, 24, false, None)
            .await
            .err()
            .expect("17th attach");
        assert!(
            error.contains("too many live tmux terminals (limit 16)"),
            "{error}"
        );
        for id in ids {
            registry.detach(owner, &id).expect("detach capped");
        }
    }

    #[tokio::test]
    async fn tmux_terminal_owner_guard_kills_on_drop() {
        let Some(session) = ThrowawaySession::create() else {
            return;
        };
        let registry = TmuxTerminalRegistry::new();
        let owner = Uuid::now_v7();
        let guard = TmuxOwnerGuard::new(Arc::clone(&registry), owner);
        let (_attached, _events) = registry
            .attach(guard.owner(), &session.name, 80, 24, false, None)
            .await
            .expect("attach");
        assert_eq!(registry.live_count(), 1);
        drop(guard);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            if registry.live_count() == 0 {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("owner drop left live terminals");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        require_session(&session.name).expect("session must survive owner drop");
    }

    #[tokio::test]
    async fn tmux_terminal_slow_drain_applies_backpressure_to_pty_reader() {
        let Some(session) = ThrowawaySession::create() else {
            return;
        };
        let registry = TmuxTerminalRegistry::new();
        let owner = Uuid::now_v7();
        let (attached, mut events) = registry
            .attach(owner, &session.name, 80, 24, false, None)
            .await
            .expect("attach");
        let payload = base64::engine::general_purpose::STANDARD
            .encode(format!("printf '{}'\r", "x".repeat(4000)));
        let flood = Arc::clone(&registry);
        let flood_id = attached.terminal_id.clone();
        let flood_task = tokio::spawn(async move {
            for _ in 0..8 {
                if flood.input(owner, &flood_id, &payload).await.is_err() {
                    break;
                }
            }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        let cap = TmuxTerminalRegistry::event_queue_capacity();
        let mut unread = 0usize;
        while events.try_recv().is_ok() {
            unread += 1;
        }
        assert!(
            unread <= cap,
            "unread event depth {unread} exceeds bound {cap}"
        );
        drop(events);
        let _ = registry.detach(owner, &attached.terminal_id);
        flood_task.abort();
        let _ = tokio::time::timeout(Duration::from_millis(200), flood_task).await;
    }

    #[tokio::test]
    async fn tmux_terminal_exit_is_last_event_after_output() {
        let Some(session) = ThrowawaySession::create() else {
            return;
        };
        let registry = TmuxTerminalRegistry::new();
        let owner = Uuid::now_v7();
        let (attached, mut events) = registry
            .attach(owner, &session.name, 80, 24, false, None)
            .await
            .expect("attach");
        let payload = base64::engine::general_purpose::STANDARD.encode("printf EXITSEQ\r");
        registry
            .input(owner, &attached.terminal_id, &payload)
            .await
            .expect("input");
        let _ = collect_output_until(&mut events, "EXITSEQ").await;
        registry
            .detach(owner, &attached.terminal_id)
            .expect("detach");
        let mut saw_exit = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(500), events.recv()).await {
                Ok(Some(TmuxTerminalEvent::TmuxTerminalOutput { .. })) => {
                    assert!(!saw_exit, "output arrived after exit");
                }
                Ok(Some(TmuxTerminalEvent::TmuxTerminalHistory { .. })) => {
                    assert!(!saw_exit, "history arrived after exit");
                }
                Ok(Some(TmuxTerminalEvent::TmuxTerminalExit { .. })) => {
                    saw_exit = true;
                }
                Ok(None) => break,
                Err(_) => {
                    if saw_exit {
                        break;
                    }
                }
            }
        }
        assert!(saw_exit, "exit must be published after buffered output");
        match events.try_recv() {
            Err(mpsc::error::TryRecvError::Empty | mpsc::error::TryRecvError::Disconnected) => {}
            Ok(TmuxTerminalEvent::TmuxTerminalOutput { .. }) => {
                panic!("output after exit on the same stream")
            }
            Ok(TmuxTerminalEvent::TmuxTerminalHistory { .. }) => {
                panic!("history after exit on the same stream")
            }
            Ok(TmuxTerminalEvent::TmuxTerminalExit { .. }) => {}
        }
    }

    #[tokio::test]
    async fn tmux_terminal_blocked_input_does_not_hold_registry_lock() {
        let Some(session) = ThrowawaySession::create() else {
            return;
        };
        let registry = TmuxTerminalRegistry::new();
        let owner = Uuid::now_v7();
        let (attached, events) = registry
            .attach(owner, &session.name, 80, 24, false, None)
            .await
            .expect("attach");
        let terminal_id = attached.terminal_id.clone();
        let payload = base64::engine::general_purpose::STANDARD.encode("x".repeat(4096));
        let writer = Arc::clone(&registry);
        let write_id = terminal_id.clone();
        let input_task = tokio::spawn(async move {
            for _ in 0..64 {
                if writer.input(owner, &write_id, &payload).await.is_err() {
                    break;
                }
            }
        });
        let _ = events;
        let detached = tokio::time::timeout(Duration::from_secs(2), async {
            registry.detach(owner, &terminal_id)
        })
        .await
        .expect("detach must not wait on a blocked PTY writer");
        detached.expect("detach");
        let _ = input_task.await;
        assert_eq!(registry.live_count(), 0);
    }

    #[tokio::test]
    async fn tmux_terminal_closed_owner_rejects_inflight_attach() {
        let Some(session) = ThrowawaySession::create() else {
            return;
        };
        let registry = TmuxTerminalRegistry::new();
        let owner = Uuid::now_v7();
        registry.kill_owner(owner);
        let error = registry
            .attach(owner, &session.name, 80, 24, false, None)
            .await
            .err()
            .expect("closed owner");
        assert!(
            error.contains("tmux terminal owner is closed"),
            "{error}"
        );
        assert_eq!(registry.live_count(), 0);

        let owner = Uuid::now_v7();
        let attaching = Arc::clone(&registry);
        let name = session.name.clone();
        let task = tokio::spawn(async move { attaching.attach(owner, &name, 80, 24, false, None).await });
        registry.kill_owner(owner);
        match task.await.expect("attach task") {
            Ok((attached, _events)) => {
                assert_eq!(registry.live_count(), 0);
                let _ = registry.detach(owner, &attached.terminal_id);
            }
            Err(error) => {
                assert!(
                    error.contains("tmux terminal owner is closed"),
                    "{error}"
                );
                assert_eq!(registry.live_count(), 0);
            }
        }
    }

    #[tokio::test]
    async fn tmux_terminal_natural_client_exit_publishes_exit_last() {
        let Some(session) = ThrowawaySession::create() else {
            return;
        };
        let registry = TmuxTerminalRegistry::new();
        let owner = Uuid::now_v7();
        let (attached, mut events) = registry
            .attach(owner, &session.name, 80, 24, false, None)
            .await
            .expect("attach");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let client_pid = loop {
            let pids = session_client_pids(&session.name);
            if let Some(pid) = pids.first().copied() {
                break pid;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("attach did not produce a tmux client");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        let payload = base64::engine::general_purpose::STANDARD.encode("printf NATURALSEQ\r");
        registry
            .input(owner, &attached.terminal_id, &payload)
            .await
            .expect("input");
        let _ = collect_output_until(&mut events, "NATURALSEQ").await;
        let killed = Command::new("kill")
            .args(["-TERM", &client_pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("kill client");
        assert!(killed.success(), "kill client {client_pid}");
        let mut saw_exit = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(500), events.recv()).await {
                Ok(Some(TmuxTerminalEvent::TmuxTerminalOutput { .. })) => {
                    assert!(!saw_exit, "output arrived after exit");
                }
                Ok(Some(TmuxTerminalEvent::TmuxTerminalHistory { .. })) => {
                    assert!(!saw_exit, "history arrived after exit");
                }
                Ok(Some(TmuxTerminalEvent::TmuxTerminalExit { .. })) => {
                    saw_exit = true;
                }
                Ok(None) => break,
                Err(_) => {
                    if saw_exit {
                        break;
                    }
                }
            }
        }
        assert!(saw_exit, "natural client exit must publish tmux_terminal_exit");
        match events.try_recv() {
            Err(mpsc::error::TryRecvError::Empty | mpsc::error::TryRecvError::Disconnected) => {}
            Ok(TmuxTerminalEvent::TmuxTerminalOutput { .. }) => {
                panic!("output after exit on the same stream")
            }
            Ok(TmuxTerminalEvent::TmuxTerminalHistory { .. }) => {
                panic!("history after exit on the same stream")
            }
            Ok(TmuxTerminalEvent::TmuxTerminalExit { .. }) => {}
        }
        let live_deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            if registry.live_count() == 0 {
                break;
            }
            if tokio::time::Instant::now() >= live_deadline {
                panic!("natural exit left a live terminal");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        require_session(&session.name).expect("session must survive client death");
        assert_eq!(session_attached_count(&session.name), 0);
    }

    #[tokio::test]
    async fn tmux_terminal_resize_does_not_hold_registry_lock() {
        let Some(session) = ThrowawaySession::create() else {
            return;
        };
        let registry = TmuxTerminalRegistry::new();
        let owner = Uuid::now_v7();
        let (attached, events) = registry
            .attach(owner, &session.name, 80, 24, false, None)
            .await
            .expect("attach");
        let terminal_id = attached.terminal_id.clone();
        let resizer = Arc::clone(&registry);
        let resize_id = terminal_id.clone();
        let resize_task = tokio::spawn(async move {
            for _ in 0..8 {
                let _ = resizer.resize(owner, &resize_id, 100, 30, false).await;
            }
        });
        let _ = events;
        let detached = tokio::time::timeout(Duration::from_secs(2), async {
            registry.detach(owner, &terminal_id)
        })
        .await
        .expect("detach must not wait on PTY resize");
        detached.expect("detach");
        let _ = resize_task.await;
        assert_eq!(registry.live_count(), 0);
    }

    #[tokio::test]
    async fn tmux_terminal_cancelled_attach_leaves_no_child() {
        let Some(session) = ThrowawaySession::create() else {
            return;
        };
        let registry = TmuxTerminalRegistry::new();
        let owner = Uuid::now_v7();
        let attaching = Arc::clone(&registry);
        let name = session.name.clone();
        let task = tokio::spawn(async move { attaching.attach(owner, &name, 80, 24, false, None).await });
        let wait_inflight = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            if registry.inflight_count() > 0 {
                task.abort();
                break;
            }
            if task.is_finished() {
                match task.await {
                    Ok(Ok((attached, _events))) => {
                        registry
                            .detach(owner, &attached.terminal_id)
                            .expect("detach finished attach");
                    }
                    Ok(Err(_)) | Err(_) => {}
                }
                break;
            }
            if tokio::time::Instant::now() >= wait_inflight {
                task.abort();
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            if registry.live_count() == 0
                && registry.inflight_count() == 0
                && session_attached_count(&session.name) == 0
            {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!(
                    "cancelled attach leaked live={} inflight={} clients={}",
                    registry.live_count(),
                    registry.inflight_count(),
                    session_attached_count(&session.name)
                );
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        require_session(&session.name).expect("session must survive cancelled attach");
    }

    #[tokio::test]
    async fn tmux_terminal_cancelled_attach_after_spawn_reaps_child() {
        let Some(session) = ThrowawaySession::create() else {
            return;
        };
        let registry = TmuxTerminalRegistry::new();
        let owner = Uuid::now_v7();
        ATTACH_HOLD_RELEASE.store(false, Ordering::SeqCst);
        ATTACH_BEFORE_RECV_READY.store(false, Ordering::SeqCst);
        *HOLD_RECV_OWNER.lock() = Some(owner);
        struct ReleaseHold;
        impl Drop for ReleaseHold {
            fn drop(&mut self) {
                ATTACH_HOLD_RELEASE.store(true, Ordering::SeqCst);
                *HOLD_RECV_OWNER.lock() = None;
            }
        }
        let _release = ReleaseHold;
        let attaching = Arc::clone(&registry);
        let name = session.name.clone();
        let task = tokio::spawn(async move { attaching.attach(owner, &name, 80, 24, false, None).await });
        let wait_ready = tokio::time::Instant::now() + Duration::from_secs(10);
        while !ATTACH_BEFORE_RECV_READY.load(Ordering::SeqCst) {
            if tokio::time::Instant::now() >= wait_ready {
                panic!("attach did not reach post-spawn recv hold");
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let wait_child = tokio::time::Instant::now() + Duration::from_secs(10);
        let child_pid = loop {
            if let Some(pid) = spawned_child_pid(owner) {
                break pid;
            }
            let clients = session_client_pids(&session.name);
            if let Some(pid) = clients.first().copied() {
                break pid;
            }
            if tokio::time::Instant::now() >= wait_child {
                panic!("spawn hold passed without a child pid");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        task.abort();
        let _ = task.await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let clients = session_attached_count(&session.name);
            let live = registry.live_count();
            let inflight = registry.inflight_count();
            let zombie = process_is_zombie(child_pid);
            let alive = process_is_alive(child_pid);
            if live == 0 && inflight == 0 && clients == 0 && !alive && !zombie {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!(
                    "cancelled after spawn leaked live={live} inflight={inflight} clients={clients} pid={child_pid} alive={alive} zombie={zombie}"
                );
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        require_session(&session.name).expect("session must survive cancelled attach");
    }

    #[tokio::test]
    async fn tmux_terminal_post_spawn_setup_failure_reaps_child() {
        let Some(session) = ThrowawaySession::create() else {
            return;
        };
        let registry = TmuxTerminalRegistry::new();
        let owner = Uuid::now_v7();
        *FAIL_SETUP_OWNER.lock() = Some(owner);
        struct ClearFail;
        impl Drop for ClearFail {
            fn drop(&mut self) {
                *FAIL_SETUP_OWNER.lock() = None;
            }
        }
        let _clear = ClearFail;
        let session_name = session.name.clone();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(10000);
        let mut last = String::new();
        loop {
            match registry.attach(owner, &session_name, 80, 24, false, None).await {
                Err(error) if error.contains("tmux terminal pty setup failed") => break,
                Err(error) => last = error,
                Ok((attached, _events)) => {
                    let _ = registry.detach(owner, &attached.terminal_id);
                    last = "attach succeeded".to_owned();
                }
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("timed out waiting for pty setup failure last={last}");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let mut child_pid = 0;
        wait_until("spawned child pid after setup failure", || {
            match spawned_child_pid(owner) {
                Some(pid) => {
                    child_pid = pid;
                    Ok(())
                }
                None => Err("spawned child pid missing".to_owned()),
            }
        })
        .await;
        wait_until("post-spawn setup child reaped", || {
            let clients = match try_session_attached_count(&session.name) {
                Some(count) => count,
                None => return Err("list-clients not ready".to_owned()),
            };
            let live = registry.live_count();
            let inflight = registry.inflight_count();
            let state = process_state(child_pid);
            if live == 0 && inflight == 0 && clients == 0 && state.is_none() {
                return Ok(());
            }
            Err(format!(
                "live={live} inflight={inflight} clients={clients} pid={child_pid} state={state:?}"
            ))
        })
        .await;
        require_session(&session.name).expect("session must survive setup failure");
    }

    #[tokio::test]
    async fn tmux_terminal_input_timeout_detaches_wedged_writer() {
        let Some(session) = ThrowawaySession::create() else {
            return;
        };
        let registry = TmuxTerminalRegistry::new();
        let owner = Uuid::now_v7();
        let _stall = StallWriter::arm(owner);
        let (attached, events) = registry
            .attach(owner, &session.name, 80, 24, false, None)
            .await
            .expect("attach");
        let _ = events;
        let payload = base64::engine::general_purpose::STANDARD.encode("x");
        let cap = TmuxTerminalRegistry::write_queue_capacity();
        for _ in 0..cap {
            registry
                .input(owner, &attached.terminal_id, &payload)
                .await
                .expect("fill writer queue");
        }
        let error = registry
            .input(owner, &attached.terminal_id, &payload)
            .await
            .expect_err("wedged writer");
        assert!(
            error.starts_with(TMUX_TERMINAL_WRITE_TIMED_OUT),
            "{error}"
        );
        wait_until("wedged writer detached", || {
            if registry.live_count() == 0 {
                Ok(())
            } else {
                Err(format!("live={}", registry.live_count()))
            }
        })
        .await;
    }

    #[tokio::test]
    async fn tmux_terminal_full_writer_queue_does_not_stall_detach() {
        let Some(session) = ThrowawaySession::create() else {
            return;
        };
        let registry = TmuxTerminalRegistry::new();
        let owner = Uuid::now_v7();
        let _stall = StallWriter::arm(owner);
        let (attached, events) = registry
            .attach(owner, &session.name, 80, 24, false, None)
            .await
            .expect("attach");
        let _ = events;
        let payload = base64::engine::general_purpose::STANDARD.encode("x");
        let cap = TmuxTerminalRegistry::write_queue_capacity();
        for _ in 0..cap {
            registry
                .input(owner, &attached.terminal_id, &payload)
                .await
                .expect("fill writer queue");
        }
        let started = Instant::now();
        let overflow = registry
            .input(owner, &attached.terminal_id, &payload)
            .await;
        if let Err(error) = overflow {
            assert!(
                error.starts_with(TMUX_TERMINAL_WRITE_TIMED_OUT),
                "{error}"
            );
        }
        let _ = registry.detach(owner, &attached.terminal_id);
        assert!(
            started.elapsed() < Duration::from_millis(200),
            "full-queue input/detach stalled {:?}",
            started.elapsed()
        );
        wait_until("full queue detached", || {
            if registry.live_count() == 0 {
                Ok(())
            } else {
                Err(format!("live={}", registry.live_count()))
            }
        })
        .await;
    }

    #[tokio::test]
    async fn tmux_terminal_cancelled_attach_after_insert_reaps_child() {
        let Some(session) = ThrowawaySession::create() else {
            return;
        };
        let registry = TmuxTerminalRegistry::new();
        let owner = Uuid::now_v7();
        ATTACH_AFTER_INSERT_RELEASE.store(false, Ordering::SeqCst);
        ATTACH_AFTER_INSERT_READY.store(false, Ordering::SeqCst);
        *HOLD_AFTER_INSERT_OWNER.lock() = Some(owner);
        struct ReleaseHold;
        impl Drop for ReleaseHold {
            fn drop(&mut self) {
                ATTACH_AFTER_INSERT_RELEASE.store(true, Ordering::SeqCst);
                *HOLD_AFTER_INSERT_OWNER.lock() = None;
            }
        }
        let _release = ReleaseHold;
        let attaching = Arc::clone(&registry);
        let name = session.name.clone();
        let task = tokio::spawn(async move { attaching.attach(owner, &name, 80, 24, false, None).await });
        let wait_ready = tokio::time::Instant::now() + Duration::from_secs(10);
        while !ATTACH_AFTER_INSERT_READY.load(Ordering::SeqCst) {
            if tokio::time::Instant::now() >= wait_ready {
                panic!("attach did not reach post-insert hold");
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let child_pid = spawned_child_pid(owner).expect("insert hold must have a child");
        task.abort();
        let _ = task.await;
        wait_until("cancelled after insert reaped", || {
            let clients = match try_session_attached_count(&session.name) {
                Some(count) => count,
                None => return Err("list-clients not ready".to_owned()),
            };
            let live = registry.live_count();
            let inflight = registry.inflight_count();
            let state = process_state(child_pid);
            if live == 0 && inflight == 0 && clients == 0 && state.is_none() {
                return Ok(());
            }
            Err(format!(
                "live={live} inflight={inflight} clients={clients} pid={child_pid} state={state:?}"
            ))
        })
        .await;
        require_session(&session.name).expect("session must survive cancelled attach");
    }
}
