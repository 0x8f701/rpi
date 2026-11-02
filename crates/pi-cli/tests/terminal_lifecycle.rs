//! PTY lifecycle tests for the `rpi` TUI terminal guard.
//!
//! Spawns the `rpi` binary on a pseudoterminal and verifies that raw mode and
//! cursor ownership are restored across clean exit, initialization error,
//! panic, SIGTERM, SIGHUP, and suspend/resume. The default TUI must never enter
//! the alternate screen, so ordinary transcript output remains in scrollback.
//!
//! The `rpi` TUI is selected by giving the child a PTY as stdout (`is_terminal`
//! is true) and `--model faux/faux-1` so `build_session` succeeds with the
//! built-in faux provider and no network/auth. The environment is cleared so
//! no inherited provider API key can accidentally satisfy auth gating.

#![cfg(unix)]

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use nix::pty::{Winsize, openpty};
use nix::sys::signal::{Signal, kill};
use nix::sys::termios::Termios;
use nix::unistd::Pid;
use tempfile::TempDir;
use unicode_width::UnicodeWidthChar;

const ENTER_ALT: &str = "\x1b[?1049h";
const HIDE_CURSOR: &str = "\x1b[?25l";
const SHOW_CURSOR: &str = "\x1b[?25h";
const CLEAR_AFTER_CURSOR: &str = "\x1b[J";
const ENABLE_BRACKETED_PASTE: &str = "\x1b[?2004h";
const DISABLE_BRACKETED_PASTE: &str = "\x1b[?2004l";
const BRACKETED_PASTE_START: &str = "\x1b[200~";
const BRACKETED_PASTE_END: &str = "\x1b[201~";
const DISABLE_LINE_WRAP: &str = "\x1b[?7l";
const ENABLE_LINE_WRAP: &str = "\x1b[?7h";
const CLEAR_CURRENT_LINE: &str = "\x1b[2K";

/// Ctrl+D byte. In raw mode crossterm maps 0x04 to `Char('d') + CONTROL`, which
/// the keybindings resolve to `Action::Quit`; with an empty editor the TUI
/// exits cleanly.
const CTRL_D: u8 = 0x04;
/// Ctrl+C byte. Crossterm maps 0x03 to `Char('c') + CONTROL` → `Action::ClearEditor`.
/// Idle first press arms a 500ms double-press exit; second press exits cleanly.
const CTRL_C: u8 = 0x03;
/// Ctrl+O byte. Crossterm maps 0x0f to `Char('o') + CONTROL`, which toggles
/// compact and expanded ToolCard rows through the production keybinding.
const CTRL_O: u8 = 0x0f;

fn rpi_bin() -> String {
    env!("CARGO_BIN_EXE_rpi").to_owned()
}
const RESTORED_SCROLL_SESSION_ID: &str = "restored-scroll-tool-card";
const OLDEST_SCROLL_SENTINEL: &str = "OLDEST-CJK-SCROLL-SENTINEL 你学习下pi-coding-agent";
const MERMAID_SCROLL_SENTINEL: &str = "MERMAID-CJK-SCROLL-SENTINEL";
const MERMAID_CJK_SEQUENCE_SENTINEL: &str = "loop 每个模型回合";
const FLOWCHART_SCROLL_SENTINEL: &str = "FLOWCHART-CJK-SCROLL-SENTINEL";
const FLOWCHART_CJK_NEEDLE: &str = "启动任务";
const CLASS_SCROLL_SENTINEL: &str = "CLASS-CJK-SCROLL-SENTINEL";
const CLASS_CJK_NEEDLE: &str = "执行任务";
const STATE_SCROLL_SENTINEL: &str = "STATE-INLINE-CJK-SCROLL-SENTINEL";
const STATE_CJK_NEEDLE: &str = "返回错误文案";
const MERMAID_TOPOLOGY_SCREEN_SENTINEL: &str = "MERMAID-TOPOLOGY-SCREEN-SENTINEL";
const MERMAID_QUOTED_LABEL_NEEDLE: &str = "message{role, content[{type:text,text}]}";
const MERMAID_STATE_LABEL_NEEDLE: &str = "(含 is_error)";
const MERMAID_SEQUENCE_LABEL_NEEDLE: &str = "渲染结果";
const MERMAID_EDGE_LABEL_NEEDLE: &str = "session_store.rs:1963-2153";
const MERMAID_DOTTED_LABEL_NEEDLE: &str = "tentative";
const ER_SCROLL_SENTINEL: &str = "ER-CJK-SCROLL-SENTINEL";
const ER_CJK_NEEDLE: &str = "拥有";
const TOOL_CARD_TITLE_SENTINEL: &str = "● tool-fixture.rs · 8 returned";
const TOOL_CARD_OUTPUT_SENTINEL: &str = "工具输出你好";
const TAIL_SCROLL_SENTINEL: &str = "TAIL-CJK-SCROLL-SENTINEL";

fn seed_restored_scroll_session(session_dir: &Path, cwd: &Path) -> PathBuf {
    use pi_ai::{
        AssistantMessage, ContentBlock, Message, Model, StopReason, ToolCall, ToolResultMessage,
    };

    let mut model = Model::default();
    model.id = "faux-1".to_owned();
    model.api = "faux".into();
    model.provider = "faux".into();
    let recorder = pi_coding::start_session_in(
        cwd,
        Some(&model),
        Some("off"),
        Some(session_dir),
        Some(RESTORED_SCROLL_SESSION_ID),
        None,
    )
    .expect("start restored session recorder");
    let call_id = "restored-read-call";
    for turn in 0..82i64 {
        let user_text = if turn == 0 {
            OLDEST_SCROLL_SENTINEL.to_owned()
        } else if turn == 81 {
            format!("restored user {turn:03} 中文宽字符 {TAIL_SCROLL_SENTINEL}")
        } else {
            format!("restored user {turn:03} 中文宽字符")
        };
        recorder
            .record_message(&Message::user_text(user_text, turn * 4))
            .expect("record restored user message");
        if turn == 40 {
            let mut tool_call = AssistantMessage::pending(&model);
            tool_call.content = vec![ContentBlock::ToolCall(ToolCall {
                id: call_id.to_owned(),
                name: "read".to_owned(),
                arguments: serde_json::json!({"path": "tool-fixture.rs"}),
                thought_signature: None,
            })];
            tool_call.stop_reason = StopReason::ToolUse;
            tool_call.timestamp = turn * 4 + 1;
            recorder
                .record_message(&Message::Assistant(tool_call))
                .expect("record restored assistant tool call");
            recorder
                .record_message(&Message::ToolResult(ToolResultMessage {
                    tool_call_id: call_id.to_owned(),
                    tool_name: "read".to_owned(),
                    content: vec![ContentBlock::text(
                        "\u{1b}[31m工具输出你好\u{1b}[0m\nline 2\nline 3\nline 4\nline 5\nline 6\nline 7\nline 8",
                    )],
                    usage: None,
                    details: None,
                    added_tool_names: Vec::new(),
                    is_error: false,
                    timestamp: turn * 4 + 2,
                }))
                .expect("record restored tool result");
            let mut mermaid = AssistantMessage::pending(&model);
            mermaid.content = vec![ContentBlock::text(format!(
                "{MERMAID_SCROLL_SENTINEL}\n```mermaid\nsequenceDiagram\n    actor U as 用户\n    participant AP as Application\n    participant SE as Session\n    loop 每个模型回合\n        U->>AP: 提交提示\n        AP->>SE: 执行会话\n        SE-->>U: 渲染结果\n    end\n```"
            ))];
            mermaid.stop_reason = StopReason::Stop;
            mermaid.timestamp = turn * 4 + 3;
            recorder
                .record_message(&Message::Assistant(mermaid))
                .expect("record restored Mermaid assistant");
        } else if matches!(turn, 28 | 32 | 48 | 52) {
            let (sentinel, diagram) = match turn {
                28 => (
                    FLOWCHART_SCROLL_SENTINEL,
                    "flowchart LR\nA[启动任务] --> B{检查状态}\nB -->|完成| C[结束]",
                ),
                32 => (
                    CLASS_SCROLL_SENTINEL,
                    "classDiagram\nclass Application {\n+运行() 执行任务\n+检查状态() : boolean\n}\nApplication --> Database : 持久化",
                ),
                48 => (
                    STATE_SCROLL_SENTINEL,
                    "stateDiagram-v2\nstate Overlap\nOverlap --> Err[\"is_error=true<br/>返回错误文案\"]",
                ),
                52 => (
                    ER_SCROLL_SENTINEL,
                    "erDiagram\nUSER ||--o{ SESSION : 拥有\nUSER {\nstring name 名称\n}\nSESSION {\nstring id 标识\n}",
                ),
                _ => unreachable!("diagram turns are 28/32/48/52"),
            };
            let mut mermaid = AssistantMessage::pending(&model);
            mermaid.content = vec![ContentBlock::text(format!(
                "{sentinel}\n```mermaid\n{diagram}\n```"
            ))];
            mermaid.stop_reason = StopReason::Stop;
            mermaid.timestamp = turn * 4 + 1;
            recorder
                .record_message(&Message::Assistant(mermaid))
                .expect("record restored Mermaid assistant");
        } else {
            let mut assistant = AssistantMessage::pending(&model);
            assistant.content = vec![ContentBlock::text(format!(
                "restored assistant {turn:03} 中文响应"
            ))];
            assistant.stop_reason = StopReason::Stop;
            assistant.timestamp = turn * 4 + 1;
            recorder
                .record_message(&Message::Assistant(assistant))
                .expect("record restored assistant message");
        }
    }
    recorder.persist_now().expect("persist restored session");
    let session_path = recorder.path();
    let messages = pi_coding::load_session_messages(&session_path)
        .expect("load typed restored session fixture");
    assert_eq!(
        messages
            .iter()
            .filter(|message| matches!(message, Message::User(_)))
            .count(),
        82,
        "fixture must contain every restored user turn"
    );
    let all_text = messages
        .iter()
        .flat_map(|message| match message {
            Message::User(message) => message.content.iter(),
            Message::Assistant(message) => message.content.iter(),
            Message::ToolResult(message) => message.content.iter(),
            _ => [].iter(),
        })
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(all_text.contains(OLDEST_SCROLL_SENTINEL));
    assert!(all_text.contains(TAIL_SCROLL_SENTINEL));
    assert!(all_text.contains(MERMAID_SCROLL_SENTINEL));
    assert!(all_text.contains(MERMAID_CJK_SEQUENCE_SENTINEL));
    assert!(all_text.contains(FLOWCHART_SCROLL_SENTINEL));
    assert!(all_text.contains(FLOWCHART_CJK_NEEDLE));
    assert!(all_text.contains(CLASS_SCROLL_SENTINEL));
    assert!(all_text.contains(CLASS_CJK_NEEDLE));
    assert!(all_text.contains(STATE_SCROLL_SENTINEL));
    assert!(all_text.contains(STATE_CJK_NEEDLE));
    assert!(all_text.contains(ER_SCROLL_SENTINEL));
    assert!(all_text.contains(ER_CJK_NEEDLE));
    assert_eq!(
        messages
            .iter()
            .filter_map(Message::as_assistant)
            .flat_map(|message| message.content.iter())
            .filter(|block| matches!(block, ContentBlock::ToolCall(call) if call.id == call_id && call.name == "read"))
            .count(),
        1,
        "fixture must contain one matching read ToolCall"
    );
    assert_eq!(
        messages
            .iter()
            .filter(|message| matches!(message, Message::ToolResult(result) if result.tool_call_id == call_id && result.tool_name == "read"))
            .count(),
        1,
        "fixture must contain one matching read ToolResult"
    );
    recorder.close().expect("close restored session recorder");
    session_path
}
fn seed_mermaid_topology_session(session_dir: &Path, cwd: &Path) -> PathBuf {
    use pi_ai::{AssistantMessage, ContentBlock, Message, Model, StopReason};

    let mut model = Model::default();
    model.id = "faux-1".to_owned();
    model.api = "faux".into();
    model.provider = "faux".into();
    let recorder = pi_coding::start_session_in(
        cwd,
        Some(&model),
        Some("off"),
        Some(session_dir),
        Some("mermaid-topology-screen"),
        None,
    )
    .expect("start Mermaid topology session");
    recorder
        .record_message(&Message::user_text("render Mermaid topology", 1))
        .expect("record Mermaid topology prompt");
    let mut assistant = AssistantMessage::pending(&model);
    assistant.content = vec![ContentBlock::text(format!(
        "{MERMAID_TOPOLOGY_SCREEN_SENTINEL}\n\
```mermaid\n\
flowchart LR\n\
M[\"message<br/>id, parentId, timestamp,<br/>message{{role, content[{{type:text,text}}]}}\" ] --> N[下一步]\n\
```\n\
```mermaid\n\
stateDiagram-v2\n\
[*] --> Execute\n\
Execute --> AppendResult: toolResultMessage<br/>(含 is_error)\n\
AppendResult --> [*]\n\
```\n\
```mermaid\n\
sequenceDiagram\n\
participant U as 用户\n\
participant A as Application\n\
U->>A: 提交提示<br/>包含 CJK\n\
A-->>U: 渲染结果\n\
```\n\
```mermaid\n\
flowchart TD\n\
CAT[\"列表<br/>维护\"] -->|\"list/latest/prune<br/>session_store.rs:1963-2153\"| UI[\"用户<br/>界面\"]\n\
T -.tentative.-> H\n\
```"
    ))];
    assistant.stop_reason = StopReason::Stop;
    assistant.timestamp = 2;
    recorder
        .record_message(&Message::Assistant(assistant))
        .expect("record Mermaid topology response");
    recorder
        .persist_now()
        .expect("persist Mermaid topology session");
    let path = recorder.path();
    recorder.close().expect("close Mermaid topology session");
    path
}

/// An `rpi` subprocess attached to a fresh pseudoterminal, with a background
/// reader draining the master into a shared buffer.
struct PtyProbe {
    child: std::process::Child,
    writer: std::fs::File,
    buffer: Arc<Mutex<String>>,
    rows: usize,
    cols: usize,
    _home: TempDir,
    _cwd: TempDir,
}

impl PtyProbe {
    /// Spawn `rpi` with `args` on a PTY. `extra_env` augments a minimal,
    /// sanitized environment (HOME/PATH/TERM plus RPI_OFFLINE and
    /// RPI_SKIP_VERSION_CHECK to avoid any network).
    fn spawn(args: &[&str], extra_env: &[(&str, &str)]) -> Self {
        Self::spawn_sized_with_seed(args, extra_env, 24, 80, |_, _| {})
    }

    fn spawn_sized_with_seed(
        args: &[&str],
        extra_env: &[(&str, &str)],
        rows: u16,
        cols: u16,
        seed: impl FnOnce(&Path, &Path),
    ) -> Self {
        let home = TempDir::new().expect("temp HOME");
        let cwd = TempDir::new().expect("temp cwd");
        seed(home.path(), cwd.path());
        let winsize = Winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let pty = openpty(Some(&winsize), None::<&Termios>).expect("openpty");
        let slave_in = pty.slave.try_clone().expect("clone slave stdin");
        let slave_out = pty.slave.try_clone().expect("clone slave stdout");
        let slave_err = pty.slave;

        let mut cmd = Command::new(rpi_bin());
        cmd.env_clear();
        cmd.env("HOME", home.path());
        cmd.env("PATH", std::env::var("PATH").unwrap_or_default());
        cmd.env("TERM", "xterm-256color");
        cmd.env("RPI_OFFLINE", "1");
        cmd.env("RPI_SKIP_VERSION_CHECK", "1");
        for (key, value) in extra_env {
            cmd.env(key, value);
        }
        cmd.args(args);
        cmd.current_dir(cwd.path());
        cmd.stdin(Stdio::from(slave_in));
        cmd.stdout(Stdio::from(slave_out));
        cmd.stderr(Stdio::from(slave_err));

        let child = cmd.spawn().expect("spawn rpi");
        let writer = std::fs::File::from(pty.master.try_clone().expect("clone master writer"));
        let reader = std::fs::File::from(pty.master);
        let buffer = Arc::new(Mutex::new(String::new()));
        let buf = buffer.clone();
        thread::spawn(move || {
            let mut reader = reader;
            let mut chunk = [0u8; 8192];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        let bytes = &chunk[..n];
                        let s = String::from_utf8_lossy(bytes).into_owned();
                        if let Ok(mut guard) = buf.lock() {
                            guard.push_str(&s);
                        }
                        // Ratatui's inline viewport asks the terminal for its
                        // cursor position (CSI 6n). A bare PTY has no emulator,
                        // so answer like a terminal at row 1, column 1.
                        if bytes.windows(4).any(|window| window == b"\x1b[6n") {
                            let _ = reader.write_all(b"\x1b[1;1R");
                            let _ = reader.flush();
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            child,
            writer,
            buffer,
            rows: usize::from(rows),
            cols: usize::from(cols),
            _home: home,
            _cwd: cwd,
        }
    }

    fn send(&mut self, input: &[u8]) {
        self.writer.write_all(input).expect("pty write");
        self.writer.flush().expect("pty flush");
    }
    fn page_up(&mut self) {
        self.send(b"\x1b[5~");
    }

    fn page_down(&mut self) {
        self.send(b"\x1b[6~");
    }

    fn live_screen(&self) -> String {
        let retained = replay_terminal_scrollback(&self.snapshot(), self.cols, self.rows);
        retained[retained.len().saturating_sub(self.rows)..].join("\n")
    }

    fn wait_for_live(&self, timeout: Duration, predicate: impl Fn(&str) -> bool) -> Option<String> {
        let deadline = Instant::now() + timeout;
        loop {
            let live = self.live_screen();
            if predicate(&live) {
                return Some(live);
            }
            if Instant::now() >= deadline {
                return None;
            }
            thread::sleep(Duration::from_millis(25));
        }
    }

    fn wait_for_output_growth(&self, baseline: usize, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.snapshot().len() > baseline {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(25));
        }
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        let rows = rows.to_string();
        let cols = cols.to_string();
        let status = Command::new("stty")
            .args(["rows", rows.as_str(), "cols", cols.as_str()])
            .stdin(Stdio::from(
                self.writer.try_clone().expect("clone PTY for resize"),
            ))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("run stty resize");
        assert!(status.success(), "resize PTY with stty: {status:?}");
        self.rows = rows.parse().expect("rows stay numeric");
        self.cols = cols.parse().expect("cols stay numeric");
        self.signal(Signal::SIGWINCH);
    }

    fn is_running(&mut self) -> bool {
        self.child.try_wait().expect("poll rpi child").is_none()
    }

    /// Send `payload` as a bracketed paste. The TUI keeps bracketed-paste mode
    /// enabled while active, so crossterm delivers this as a single
    /// `Event::Paste(payload)` and the composer paints the whole payload in one
    /// render as a contiguous run. Per-key typing instead interleaves
    /// cursor/separator cells between characters in the raw PTY stream, so
    /// post-lifecycle composer-usability markers must be pasted, not typed.
    fn bracketed_paste(&mut self, payload: &str) {
        let mut bytes = Vec::with_capacity(
            BRACKETED_PASTE_START.len() + payload.len() + BRACKETED_PASTE_END.len(),
        );
        bytes.extend_from_slice(BRACKETED_PASTE_START.as_bytes());
        bytes.extend_from_slice(payload.as_bytes());
        bytes.extend_from_slice(BRACKETED_PASTE_END.as_bytes());
        self.send(&bytes);
    }

    fn signal(&mut self, signal: Signal) {
        let pid = Pid::from_raw(self.child.id() as i32);
        kill(pid, signal).expect("send signal to child");
    }

    fn count(&self, needle: &str) -> usize {
        self.buffer
            .lock()
            .expect("buffer lock")
            .matches(needle)
            .count()
    }

    /// Wait until `needle` appears at least `want` times in the captured
    /// output, or `timeout` elapses.
    fn wait_for_count(&self, needle: &str, want: usize, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.count(needle) >= want {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(25));
        }
    }

    /// ANSI-stripped count: the inline TUI paints styled text as
    /// per-character SGR runs, so raw matches miss visibly-present text.
    fn count_plain(&self, needle: &str) -> usize {
        let buffer = self.buffer.lock().expect("buffer lock");
        strip_ansi(&buffer).matches(needle).count()
    }

    /// Wait for `needle` at least `want` times in the ANSI-normalized
    /// output, or `timeout` elapses.
    fn wait_for_plain_count(&self, needle: &str, want: usize, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.count_plain(needle) >= want {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(25));
        }
    }

    /// Wait for `needle` strictly after a captured stream byte offset and
    /// return the offset immediately following the match. This lets lifecycle
    /// tests prove event ordering without sleeps or ambiguous global counts.
    fn wait_for_after(&self, needle: &str, start: usize, timeout: Duration) -> Option<usize> {
        let deadline = Instant::now() + timeout;
        loop {
            let found = {
                let buffer = self.buffer.lock().expect("buffer lock");
                buffer
                    .get(start..)
                    .and_then(|tail| tail.find(needle))
                    .map(|offset| start + offset + needle.len())
            };
            if found.is_some() {
                return found;
            }
            if Instant::now() >= deadline {
                return None;
            }
            thread::sleep(Duration::from_millis(25));
        }
    }

    fn snapshot(&self) -> String {
        self.buffer.lock().expect("buffer lock").clone()
    }

    /// Wait until the session recorder contains `needle`. Session persistence
    /// happens from the canonical `AgentEvent::MessageEnd`, so this is an
    /// authoritative turn-progress signal that cannot be satisfied by a live
    /// viewport repaint of an earlier turn.
    fn wait_for_recorded_message(&self, needle: &str, timeout: Duration) -> bool {
        fn tree_contains(dir: &std::path::Path, needle: &str) -> bool {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return false;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    if tree_contains(&path, needle) {
                        return true;
                    }
                } else if path.extension().and_then(|ext| ext.to_str()) == Some("jsonl")
                    && let Ok(body) = std::fs::read_to_string(&path)
                    && body.contains(needle)
                {
                    return true;
                }
            }
            false
        }

        let deadline = Instant::now() + timeout;
        loop {
            if tree_contains(self._home.path(), needle) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(25));
        }
    }

    fn wait_exit(&mut self, timeout: Duration) -> Option<std::process::ExitStatus> {
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
}

impl Drop for PtyProbe {
    fn drop(&mut self) {
        // Best effort: don't strand a child if a test failed before exit.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Wait until the TUI is fully entered and the composer chrome is rendered.
/// A rendered marker proves the event loop is ready before the test sends
/// input; startup itself performs no terminal capability reads from stdin.
/// The panic-injection test never renders a frame and must use
/// [`await_acquired`] instead.
fn await_entered(probe: &PtyProbe) -> bool {
    probe.wait_for_plain_count("faux/faux-1", 1, Duration::from_secs(30))
        || probe.wait_for_plain_count("Recent sessions", 1, Duration::from_secs(15))
}

/// Terminal acquisition only: the panic-injection test fires its panic right
/// after `TerminalGuard::enter()`, before any frame is rendered.
fn await_acquired(probe: &PtyProbe) -> bool {
    probe.wait_for_count(HIDE_CURSOR, 1, Duration::from_secs(30))
}

#[test]
fn pty_clean_exit_restores_terminal() {
    let mut probe = PtyProbe::spawn(&["--model", "faux/faux-1"], &[]);
    assert!(
        await_entered(&probe),
        "TUI must acquire the inline viewport: {}",
        probe.snapshot()
    );
    assert!(
        !probe.snapshot().contains(ENTER_ALT),
        "default TUI must not enter alternate screen"
    );
    assert!(
        probe.snapshot().contains(ENABLE_BRACKETED_PASTE),
        "TUI must enable bracketed paste while active"
    );
    assert!(
        probe.snapshot().contains(DISABLE_LINE_WRAP),
        "TUI must disable terminal autowrap while active"
    );
    probe.send(&[CTRL_D]);
    assert!(
        probe.wait_for_count(DISABLE_BRACKETED_PASTE, 1, Duration::from_secs(15)),
        "clean exit must disable bracketed paste: {}",
        probe.snapshot()
    );
    assert!(
        probe.snapshot().contains(ENABLE_LINE_WRAP),
        "clean exit must restore terminal autowrap"
    );
    assert!(
        probe.snapshot().contains(SHOW_CURSOR),
        "clean exit must restore the cursor without erasing normal output: {}",
        probe.snapshot()
    );
    let status = probe
        .wait_exit(Duration::from_secs(15))
        .expect("rpi must exit after Ctrl+D");
    assert!(
        status.success(),
        "clean exit must report success: {status:?}"
    );
    assert!(
        probe.snapshot().contains(CLEAR_AFTER_CURSOR),
        "clean exit must clear only the live inline viewport"
    );
    let output = probe.snapshot();
    let after_clear = output
        .rsplit_once(CLEAR_AFTER_CURSOR)
        .map_or("", |(_, tail)| tail);
    assert!(!after_clear.contains("faux/faux-1"));
    assert!(!after_clear.contains("ready"));
}

/// `--listen` is a Web-only service even when stdout/stderr are attached to a
/// terminal. Closed stdin cannot end it, no TUI terminal sequence is emitted,
/// `/web` and tokenless `/rpc` remain available, and SIGTERM shuts it down.
#[test]
fn pty_listener_is_headless_and_signal_owned_with_closed_stdin() {
    use std::net::{TcpListener as StdTcpListener, TcpStream};

    let home = TempDir::new().expect("temp HOME");
    let cwd = TempDir::new().expect("temp cwd");
    let port_probe = StdTcpListener::bind("127.0.0.1:0").expect("probe listener port");
    let address = port_probe.local_addr().expect("probe address");
    drop(port_probe);

    let winsize = Winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let pty = openpty(Some(&winsize), None::<&Termios>).expect("openpty");
    let slave_out = pty.slave.try_clone().expect("clone slave stdout");
    let slave_err = pty.slave;
    let mut child = Command::new(rpi_bin())
        .args([
            "--listen",
            &address.to_string(),
            "--listen-plaintext",
            "--model",
            "faux/faux-1",
            "--api-key",
            "faux",
        ])
        .env_clear()
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env("PI_CODING_AGENT_DIR", home.path().join(".pi"))
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("TERM", "xterm-256color")
        .env("RPI_OFFLINE", "1")
        .env("RPI_SKIP_VERSION_CHECK", "1")
        .env("PI_FAUX_RESPONSE", "headless-listener-reply")
        .current_dir(cwd.path())
        .stdin(Stdio::null())
        .stdout(Stdio::from(slave_out))
        .stderr(Stdio::from(slave_err))
        .spawn()
        .expect("spawn listener");
    let reader = std::fs::File::from(pty.master);
    let buffer = Arc::new(Mutex::new(Vec::<u8>::new()));
    let captured = buffer.clone();
    let reader_task = thread::spawn(move || {
        let mut reader = reader;
        let mut chunk = [0u8; 8192];
        while let Ok(count) = reader.read(&mut chunk) {
            if count == 0 {
                break;
            }
            captured
                .lock()
                .expect("capture lock")
                .extend_from_slice(&chunk[..count]);
        }
    });

    let rpc_body = br#"{"type":"get_state","id":"headless-state"}"#;
    let rpc_request = format!(
        "POST /rpc HTTP/1.1\r\nhost: {address}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        rpc_body.len()
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut rpc_response = Vec::new();
    while Instant::now() < deadline {
        if let Ok(mut stream) = TcpStream::connect_timeout(&address, Duration::from_millis(200)) {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            if stream.write_all(rpc_request.as_bytes()).is_ok()
                && stream.write_all(rpc_body).is_ok()
                && stream.read_to_end(&mut rpc_response).is_ok()
                && rpc_response
                    .windows(b"\"success\":true".len())
                    .any(|part| part == b"\"success\":true")
            {
                break;
            }
            rpc_response.clear();
        }
        assert!(
            child.try_wait().expect("poll listener").is_none(),
            "closed stdin must not stop listener"
        );
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        rpc_response
            .windows(b"headless-state".len())
            .any(|part| part == b"headless-state"),
        "tokenless RPC must be served"
    );

    let mut web = TcpStream::connect(address).expect("connect /web");
    web.write_all(
        format!("GET /web HTTP/1.1\r\nhost: {address}\r\nconnection: close\r\n\r\n").as_bytes(),
    )
    .expect("write /web request");
    let mut web_response = Vec::new();
    web.read_to_end(&mut web_response)
        .expect("read /web response");
    assert!(
        web_response.starts_with(b"HTTP/1.1 200"),
        "/web must return 200"
    );
    assert!(
        web_response
            .windows(b"<!doctype html".len())
            .any(|part| part == b"<!doctype html")
    );
    assert!(
        child.try_wait().expect("poll after Web probes").is_none(),
        "listener must remain alive"
    );

    kill(Pid::from_raw(child.id() as i32), Signal::SIGTERM).expect("send SIGTERM");
    let exit_deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll SIGTERM exit") {
            break status;
        }
        assert!(
            Instant::now() < exit_deadline,
            "listener did not stop after SIGTERM"
        );
        thread::sleep(Duration::from_millis(25));
    };
    assert!(status.success(), "SIGTERM listener exit: {status:?}");
    reader_task.join().expect("join PTY reader");

    let output = String::from_utf8_lossy(&buffer.lock().expect("capture lock")).into_owned();
    assert!(
        output.contains("Control plane listening"),
        "startup URL missing: {output}"
    );
    for forbidden in [
        ENTER_ALT,
        HIDE_CURSOR,
        ENABLE_BRACKETED_PASTE,
        DISABLE_LINE_WRAP,
        "\x1b[6n",
        "Recent sessions",
        "cursor position could not be read",
    ] {
        assert!(
            !output.contains(forbidden),
            "headless listener emitted terminal/TUI marker {forbidden:?}: {output}"
        );
    }
}

#[test]
fn pty_startup_preserves_input_sent_after_terminal_acquisition() {
    let mut probe = PtyProbe::spawn(&["--model", "faux/faux-1"], &[]);
    assert!(
        await_acquired(&probe),
        "TUI must acquire the terminal before startup input: {}",
        probe.snapshot()
    );
    probe.bracketed_paste("startup-keystroke");
    assert!(
        probe.wait_for_plain_count("startup-keystroke", 1, Duration::from_secs(30)),
        "the event loop must render every byte typed during startup: {}",
        probe.snapshot()
    );
    probe.send(&[CTRL_C, CTRL_C]);
    let status = probe
        .wait_exit(Duration::from_secs(15))
        .expect("rpi must exit after Ctrl+C twice");
    assert!(
        status.success(),
        "startup-input probe must exit cleanly: {status:?}"
    );
}

#[test]
fn pty_settings_overlay_escape_does_not_retain_settings_in_scrollback() {
    // Open /settings on the normal-screen inline TUI, dismiss with Escape, keep
    // interacting, then quit. The PTY capture is the native scrollback surface:
    // settings chrome must not remain after dismiss while ordinary conversation
    // markers stay available.
    let mut probe = PtyProbe::spawn(&["--model", "faux/faux-1"], &[]);
    assert!(
        await_entered(&probe),
        "TUI must acquire the inline viewport: {}",
        probe.snapshot()
    );
    assert!(
        !probe.snapshot().contains(ENTER_ALT),
        "settings path must stay on the normal screen"
    );
    // Wait until the idle composer is painted so slash input is accepted.
    assert!(
        probe.wait_for_count("faux/faux-1", 1, Duration::from_secs(20)),
        "composer model chrome must render before /settings: {}",
        probe.snapshot()
    );

    probe.send(b"/settings\r");
    assert!(
        probe.wait_for_count("Settings", 1, Duration::from_secs(20)),
        "settings overlay must render: {}",
        probe.snapshot()
    );
    // The settings footer is wider than the 80-column viewport, so the apply
    // and scope chords near its end ("Ctrl-S apply", "type to filter") are
    // truncated and never painted. "Del reset" appears in both footer variants
    // and lands within the viewport, so it is the observable proof the overlay
    // actually rendered. Wait for it through the PTY capture before dismissing
    // instead of an arbitrary sleep.
    assert!(
        wait_for_live_viewport_contains(&probe, "Del reset", Duration::from_secs(20)),
        "settings footer chrome must be painted before Escape: {}",
        probe.snapshot()
    );

    // Escape dismisses the page overlay; clear-on-dismiss erases live pixels.
    // Observe the chrome actually leaving the live viewport before continuing.
    probe.send(b"\x1b");
    assert!(
        wait_for_live_viewport_absent(&probe, "Del reset", Duration::from_secs(10)),
        "Escape must clear the settings overlay from the live viewport: {}",
        probe.snapshot()
    );

    // Continue without submitting a model turn (avoid faux prompt races). Type
    // into the composer then wipe with Ctrl-C so any dirty overlay rows would
    // still have had a chance to be committed if the dismiss clear were missing.
    probe.send(b"hello after settings");
    assert!(
        wait_for_live_viewport_contains(&probe, "hello after settings", Duration::from_secs(10)),
        "post-settings composer input must render: {}",
        probe.snapshot()
    );
    probe.send(&[CTRL_C]);
    assert!(
        wait_for_live_viewport_absent(&probe, "hello after settings", Duration::from_secs(10)),
        "Ctrl-C must clear the composer input: {}",
        probe.snapshot()
    );

    probe.send(&[CTRL_D]);
    assert!(
        probe.wait_for_count(SHOW_CURSOR, 1, Duration::from_secs(15)),
        "clean exit after settings must restore the cursor: {}",
        probe.snapshot()
    );
    let status = probe
        .wait_exit(Duration::from_secs(15))
        .expect("rpi must exit after Ctrl+D");
    assert!(
        status.success(),
        "exit after settings dismiss must succeed: {status:?}\nsnapshot:\n{}",
        probe.snapshot()
    );

    let output = probe.snapshot();
    assert!(
        !output.contains(ENTER_ALT),
        "settings flow must never enter the alternate screen"
    );

    // The full PTY write log still contains the open-frame paint (expected).
    // Retained scrollback is the content that survives after the dismiss clear
    // of the live inline viewport. Take the suffix after the first clear that
    // follows the open settings chrome — that is the post-Escape surface. The
    // footer is truncated at 80 columns, so anchor on chrome that is actually
    // painted within the viewport ("Del reset" / "Ctrl-G/P").
    let settings_open_at = output
        .find("Del reset")
        .or_else(|| output.find("Ctrl-G/P"))
        .expect("open settings chrome must have been painted");
    let after_open = &output[settings_open_at..];
    let post_dismiss = after_open
        .find(CLEAR_AFTER_CURSOR)
        .map(|idx| &after_open[idx + CLEAR_AFTER_CURSOR.len()..])
        .expect("Escape must clear the live inline viewport after settings");
    let row_clear_at = after_open
        .find(CLEAR_CURRENT_LINE)
        .expect("Escape must clear overlay rows before clearing the viewport");
    let viewport_clear_at = after_open
        .find(CLEAR_AFTER_CURSOR)
        .expect("Escape must reset the live viewport after row clears");
    assert!(
        row_clear_at < viewport_clear_at,
        "row-wise clearing must precede ED so tmux does not retain overlay rows"
    );
    let plain_after = strip_ansi(post_dismiss);
    let settings_needles = [
        "Ctrl-S apply",
        "Ctrl-G/Ctrl-P",
        "type to filter",
        "↑/↓ select · Enter edit/toggle",
        "Enter edit/toggle · Del reset",
        "defaultProvider",
        "thinkingBudgets",
    ];
    for needle in settings_needles {
        assert!(
            !plain_after.contains(needle),
            "settings overlay needle {needle:?} must be absent from the post-dismiss live/scroll surface.\npost_dismiss plain:\n{plain_after}\nfull plain:\n{}",
            strip_ansi(&output)
        );
    }
    // Durable conversation chrome remains reachable in the overall capture
    // (and typically is repainted after dismiss into the live region).
    let plain_all = strip_ansi(&output);
    assert!(
        plain_all.contains("faux/faux-1"),
        "durable conversation chrome must remain accessible after settings dismiss.\nplain:\n{plain_all}"
    );
    assert!(
        plain_after.contains("faux/faux-1") || plain_after.contains("hello after settings"),
        "post-dismiss surface must still show conversation chrome or continued input.\npost_dismiss plain:\n{plain_after}"
    );
}

fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            match chars.peek() {
                Some('[') => {
                    chars.next();
                    for next in chars.by_ref() {
                        if next.is_ascii_alphabetic() || next == '@' {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    while let Some(next) = chars.next() {
                        if next == '\u{7}' {
                            break;
                        }
                        if next == '\u{1b}' {
                            if chars.peek() == Some(&'\\') {
                                chars.next();
                            }
                            break;
                        }
                    }
                }
                Some(_) => {
                    let _ = chars.next();
                }
                None => {}
            }
        } else {
            out.push(ch);
        }
    }
    out
}

/// Replay the subset of ANSI terminal control used by the inline TUI into a
/// fixed-size screen plus unbounded normal-screen scrollback. This deliberately
/// models newline-at-bottom promotion, cursor-addressed paints, and the erase
/// sequences used by `clear_inline_viewport`; SGR and terminal modes do not
/// affect retained text.
fn replay_terminal_scrollback(input: &str, width: usize, height: usize) -> Vec<String> {
    // Internal marker cell for the continuation half of a double-width
    // glyph. A real terminal stores one wide glyph across both cells and
    // copy-mode yields it once; row collection below filters this marker so
    // the replayed scrollback reads each CJK glyph exactly once. A later
    // `MoveTo(col_inside_glyph)` + `Print(' ')` overwrites the marker with a
    // literal space (the WideCell placeholder bug), which then survives in
    // the replayed row as a visible gap between glyphs.
    const WIDE_CONTINUATION: char = '\u{0}';
    let width = width.max(1);
    let height = height.max(1);
    let mut screen = vec![vec![' '; width]; height];
    let mut scrollback = Vec::<String>::new();
    let mut row = 0usize;
    let mut column = 0usize;
    let chars = input.chars().collect::<Vec<_>>();
    let mut index = 0usize;

    let scroll = |screen: &mut Vec<Vec<char>>, scrollback: &mut Vec<String>| {
        scrollback.push(
            screen[0]
                .iter()
                .filter(|c| **c != WIDE_CONTINUATION)
                .collect(),
        );
        screen.rotate_left(1);
        screen[height - 1].fill(' ');
    };
    while index < chars.len() {
        match chars[index] {
            '\u{1b}' if chars.get(index + 1) == Some(&'[') => {
                index += 2;
                let start = index;
                while index < chars.len() && !(('@'..='~').contains(&chars[index])) {
                    index += 1;
                }
                if index == chars.len() {
                    break;
                }
                let final_byte = chars[index];
                let params = chars[start..index].iter().collect::<String>();
                let values = params
                    .trim_start_matches('?')
                    .split(';')
                    .map(|part| part.parse::<usize>().unwrap_or(0))
                    .collect::<Vec<_>>();
                match final_byte {
                    'H' | 'f' => {
                        row = values
                            .first()
                            .copied()
                            .unwrap_or(1)
                            .max(1)
                            .saturating_sub(1)
                            .min(height - 1);
                        column = values
                            .get(1)
                            .copied()
                            .unwrap_or(1)
                            .max(1)
                            .saturating_sub(1)
                            .min(width - 1);
                    }
                    'A' => row = row.saturating_sub(values.first().copied().unwrap_or(1).max(1)),
                    'B' => {
                        row = row
                            .saturating_add(values.first().copied().unwrap_or(1).max(1))
                            .min(height - 1)
                    }
                    'C' => {
                        column = column
                            .saturating_add(values.first().copied().unwrap_or(1).max(1))
                            .min(width - 1)
                    }
                    'D' => {
                        column = column.saturating_sub(values.first().copied().unwrap_or(1).max(1))
                    }
                    'G' => {
                        column = values
                            .first()
                            .copied()
                            .unwrap_or(1)
                            .max(1)
                            .saturating_sub(1)
                            .min(width - 1)
                    }
                    'J' => match values.first().copied().unwrap_or(0) {
                        0 => {
                            screen[row][column..].fill(' ');
                            for line in &mut screen[row + 1..] {
                                line.fill(' ');
                            }
                        }
                        1 => {
                            for line in &mut screen[..row] {
                                line.fill(' ');
                            }
                            screen[row][..=column].fill(' ');
                        }
                        2 | 3 => screen.iter_mut().for_each(|line| line.fill(' ')),
                        _ => {}
                    },
                    'K' => match values.first().copied().unwrap_or(0) {
                        0 => screen[row][column..].fill(' '),
                        1 => screen[row][..=column].fill(' '),
                        2 => screen[row].fill(' '),
                        _ => {}
                    },
                    _ => {}
                }
            }
            '\u{1b}' if chars.get(index + 1) == Some(&']') => {
                index += 2;
                while index < chars.len() {
                    if chars[index] == '\u{7}' {
                        break;
                    }
                    if chars[index] == '\u{1b}' && chars.get(index + 1) == Some(&'\\') {
                        index += 1;
                        break;
                    }
                    index += 1;
                }
            }
            '\u{1b}' => {
                index = index.saturating_add(1);
            }
            '\n' => {
                if row + 1 == height {
                    scroll(&mut screen, &mut scrollback);
                } else {
                    row += 1;
                }
            }
            '\r' => column = 0,
            ch if !ch.is_control() => {
                let char_width = ch.width().unwrap_or(0);
                if char_width > 0 {
                    screen[row][column] = ch;
                    // A double-width glyph occupies its leading cell plus the
                    // following continuation cell(s); mark them so row output
                    // collapses the glyph to one character (matching tmux
                    // copy-mode). A later MoveTo+Print(' ') that overwrites a
                    // continuation cell — the WideCell placeholder bug —
                    // replaces this marker with a literal space, breaking the
                    // glyph and leaving a visible gap in the replayed row.
                    for fill in 1..char_width {
                        let col = column.saturating_add(fill);
                        if col >= width {
                            break;
                        }
                        screen[row][col] = WIDE_CONTINUATION;
                    }
                    column = column.saturating_add(char_width).min(width - 1);
                }
            }
            _ => {}
        }
        index += 1;
    }
    scrollback.extend(screen.into_iter().map(|line| {
        line.into_iter()
            .filter(|c| *c != WIDE_CONTINUATION)
            .collect()
    }));
    scrollback
}

/// Wait until the inline composer is idle: the composer header line (the
/// `╭── π … faux/faux-1 …╮` top border) is rendered AND the OMP-style status
/// line directly above it is blank. While busy that status line carries the
/// spinner/activity text (`⟲ working` / rotating frame); idle it is empty —
/// the row is always reserved by the layout, so its emptiness is a
/// truncation-proof signal that the active `working` turn cleared and the
/// composer is usable again.
fn wait_for_idle_composer(probe: &PtyProbe, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let retained = replay_terminal_scrollback(&probe.snapshot(), 80, 24);
        let live_start = retained.len().saturating_sub(24);
        let idle = retained[live_start..].windows(2).any(|rows| {
            rows[1].contains('╭') && rows[1].contains("faux/faux-1") && rows[0].trim().is_empty()
        });
        if idle {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(25));
    }
}

/// Wait until the inline composer goes *busy* past a pre-submit baseline:
/// the OMP-style status row above the `╭── π` header paints the generic
/// `Working…` fallback the moment a faux turn streams, and the append-only
/// PTY capture retains that paint even when the streaming window is narrower
/// than the poll interval.
///
/// This is the turn-boundary signal [`PtyProbe::wait_for_after`] keyed on the
/// assistant marker cannot provide: the faux response is byte-identical
/// across turns, and the inline TUI re-emits the live viewport (including the
/// prior turn's assistant text) on every redraw, so a marker match after the
/// submit offset is satisfied by a *re-render* of the previous turn's
/// assistant — before the new turn's `AgentStart` is processed. Submitting
/// the next prompt at that point lands while the prior turn is still
/// streaming and is treated as a steering message (`StreamingBehavior::Steer`),
/// duplicating the prior user echo and dropping the new turn from the
/// transcript. A `Working` count rising past the pre-submit baseline proves
/// the new turn's `AgentStart` was processed; the subsequent
/// [`wait_for_idle_composer`] then observes its `AgentSettled` (the
/// animation-tick poll that used to race the indicator ahead of `AgentSettled`
/// was removed, so an idle status row is now proof the turn settled).
fn wait_for_busy_after(probe: &PtyProbe, baseline: usize, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if probe.count_plain("Working") > baseline {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(25));
    }
}

/// Wait until `needle` appears in the *committed* scrollback — the replayed
/// region above the live 24-row viewport — rather than merely in the live
/// paint. The inline TUI streams assistant content into the live viewport
/// first and only promotes it to native scrollback through `insert_before`
/// once the turn settles. The streaming indicator can clear just ahead of the
/// still-queued `MessageEnd` (see the animation-tick note in `tui.rs`), so
/// [`wait_for_idle_composer`] alone is not proof that the prior turn's
/// assistant was actually committed. Observing the commit through the PTY
/// capture keeps the test from exiting before the assistant content is
/// durable, without an arbitrary sleep.
fn wait_for_committed_scrollback(probe: &PtyProbe, needle: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let retained = replay_terminal_scrollback(&probe.snapshot(), 80, 24);
        let scrollback_end = retained.len().saturating_sub(24);
        if scrollback_end > 0
            && retained[..scrollback_end]
                .iter()
                .any(|line| line.contains(needle))
        {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(25));
    }
}

/// Wait until `needle` is present in the live 24-row viewport (the replayed
/// surface's last 24 rows). Used to observe an overlay actually painting its
/// chrome before dismissing it — through the PTY capture, not an arbitrary
/// sleep. The settings footer is truncated at the 80-column viewport, so the
/// apply/scope chords near its end are never painted; the caller passes a
/// chrome token that is actually rendered within the viewport.
fn wait_for_live_viewport_contains(probe: &PtyProbe, needle: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let retained = replay_terminal_scrollback(&probe.snapshot(), 80, 24);
        let live_start = retained.len().saturating_sub(24);
        if retained[live_start..]
            .iter()
            .any(|line| line.contains(needle))
        {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(25));
    }
}

/// Wait until `needle` is absent from the live 24-row viewport. Used to observe
/// a dismiss/erase actually clearing painted pixels before the next step.
fn wait_for_live_viewport_absent(probe: &PtyProbe, needle: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let retained = replay_terminal_scrollback(&probe.snapshot(), 80, 24);
        let live_start = retained.len().saturating_sub(24);
        if !retained[live_start..]
            .iter()
            .any(|line| line.contains(needle))
        {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn pty_committed_turns_never_promote_standalone_working_composer() {
    let response = "assistant-scrollback-marker ".repeat(3);
    let mut probe = PtyProbe::spawn(
        &["--model", "faux/faux-1"],
        &[("PI_FAUX_RESPONSE", response.as_str())],
    );
    assert!(
        await_entered(&probe),
        "TUI must acquire the inline viewport: {}",
        probe.snapshot()
    );
    assert!(!probe.snapshot().contains(ENTER_ALT));

    for turn in 1..=3 {
        let prompt = format!("user-scrollback-marker-{turn} ").repeat(24);
        // The faux assistant response is byte-identical across turns, and the
        // inline TUI re-emits the live viewport (including the prior turn's
        // assistant text) on every redraw, so a marker match after the submit
        // offset is satisfied by a re-render of the previous turn's assistant
        // before the new turn's `AgentStart` is processed. Submitting the next
        // prompt then lands while the prior turn is still streaming and is
        // treated as a steering message, duplicating the prior user echo and
        // dropping the new turn. Observe the new turn's `AgentStart` instead:
        // a rising `Working` count past the pre-submit baseline proves the
        // composer went busy for this turn before we wait for it to settle.
        let busy_baseline = probe.count_plain("Working");
        probe.bracketed_paste(&prompt);
        probe.send(b"\r");
        assert!(
            wait_for_busy_after(&probe, busy_baseline, Duration::from_secs(20)),
            "turn {turn} must start streaming (composer goes busy) before it can settle: {}",
            probe.snapshot()
        );
        assert!(
            wait_for_idle_composer(&probe, Duration::from_secs(20)),
            "turn {turn} must settle, commit, and redraw an idle composer before the next submission: {}",
            probe.snapshot()
        );
        assert!(
            probe.wait_for_count(
                &format!("user-scrollback-marker-{turn}"),
                1,
                Duration::from_secs(10),
            ),
            "turn {turn} user entry must be visible: {}",
            probe.snapshot()
        );
    }
    // The streaming indicator can clear ahead of the queued `MessageEnd`, so
    // an idle composer is not proof the prior turns' assistant content was
    // committed. Observe the actual `insert_before` promotion through the PTY
    // capture before exiting: at least one prior assistant turn must already
    // be durable in scrollback (the final exit flush commits any remainder).
    assert!(
        wait_for_committed_scrollback(
            &probe,
            "assistant-scrollback-marker",
            Duration::from_secs(20)
        ),
        "a prior assistant turn must commit to scrollback before exit: {}",
        probe.snapshot()
    );

    probe.send(&[CTRL_D]);
    assert!(
        probe.wait_for_count(SHOW_CURSOR, 1, Duration::from_secs(15)),
        "clean exit after committed turns must restore the cursor: {}",
        probe.snapshot()
    );
    let status = probe
        .wait_exit(Duration::from_secs(15))
        .expect("rpi must exit after committed turns");
    assert!(
        status.success(),
        "committed-turn exit must succeed: {status:?}"
    );
    let retained = replay_terminal_scrollback(&probe.snapshot(), 80, 24);
    let committed = retained
        .iter()
        .map(|line| line.trim_end())
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    for turn in 1..=3 {
        assert!(
            committed
                .iter()
                .any(|line| line.contains(&format!("user-scrollback-marker-{turn}"))),
            "committed user turn {turn} must survive terminal replay: {committed:#?}"
        );
    }
    assert!(
        committed
            .iter()
            .any(|line| line.contains("assistant-scrollback-marker")),
        "committed assistant content must survive terminal replay: {committed:#?}"
    );
    // Transient composer chrome now spans three rows — the OMP-style status
    // line (spinner/`working`), the `╭── π` header, and the `╰─` footer — and
    // none of that frame may leak into committed scrollback.
    let stale_composer = committed.windows(3).any(|rows| {
        rows[0].contains("working")
            && rows[1].contains("╭── π")
            && rows[2].trim_start().starts_with("╰─")
    });
    assert!(
        !stale_composer,
        "standalone working status+composer frame leaked into scrollback: {committed:#?}"
    );
    assert!(
        !committed
            .iter()
            .any(|line| line.contains("╭── π") && line.contains("working")),
        "working composer header leaked without its footer into scrollback: {committed:#?}"
    );
    assert!(
        !committed
            .iter()
            .any(|line| line.trim_start().starts_with("╰─")),
        "empty composer footer leaked without its header into scrollback: {committed:#?}"
    );
    assert!(probe.snapshot().contains(DISABLE_BRACKETED_PASTE));
    assert!(probe.snapshot().contains(ENABLE_LINE_WRAP));
    assert!(probe.snapshot().contains(SHOW_CURSOR));
}

/// Native scrollback regression for the WideCell placeholder bug. A mixed
/// CJK/ASCII user prompt plus a framed multiline CJK/ASCII/Markdown/Mermaid
/// assistant turn settles and is committed to native scrollback through
/// `Terminal::insert_before` — the only path that feeds the backend every
/// buffer cell, including the reset-to-space continuation cell that follows
/// each width-2 CJK glyph. The WideCell cursor tracker must skip that
/// continuation cell so the prior glyph's right half is not overwritten by a
/// leading space; otherwise every CJK glyph in the committed row is broken
/// (a visible gap between glyphs, and dropped graphemes once tmux copy-mode
/// re-selects the broken cells). The first user prompt
/// `你学习下pi-coding-agent` is the exact reported payload that lost its
/// `你学习下` prefix in copy-mode scrollback.
#[test]
fn pty_cjk_settled_turn_commits_contiguous_glyphs_to_scrollback() {
    let response = "cjk-scrollback-marker 你好世界 abc123\n\
## framed section 标题\n\
\n\
- 项目一 one\n\
- 项目二 two\n\
- 项目三 three\n\
- 项目四 four\n\
- 项目五 five\n\
- 项目六 six\n\
- 项目七 seven\n\
- 项目八 eight\n\
\n\
```mermaid\n\
flowchart LR\n\
    A[开始] --> B[处理]\n\
    B --> C[验证]\n\
    C --> D[结束]\n\
```\n";
    let mut probe = PtyProbe::spawn(
        &["--model", "faux/faux-1"],
        &[("PI_FAUX_RESPONSE", response)],
    );
    assert!(
        await_entered(&probe),
        "TUI must acquire the inline viewport: {}",
        probe.snapshot()
    );
    assert!(
        !probe.snapshot().contains(ENTER_ALT),
        "CJK scrollback path must stay on the normal screen"
    );

    // First prompt: the exact reported CJK/ASCII payload. It settles; the
    // turn is committed to native scrollback once the next turn overflows.
    let busy_baseline = probe.count_plain("Working");
    probe.bracketed_paste("你学习下pi-coding-agent");
    probe.send(b"\r");
    assert!(
        wait_for_busy_after(&probe, busy_baseline, Duration::from_secs(20)),
        "first CJK turn must start streaming before it can settle: {}",
        probe.snapshot()
    );
    // Authoritative settle prerequisite: the session recorder appends the
    // assistant only from its canonical `MessageEnd`. A live viewport marker
    // is not sufficient because redraws can re-emit an earlier turn while the
    // application broadcast queue is still draining. Pairing the durable
    // message with the idle composer below proves both MessageEnd persistence
    // and TUI-side AgentSettled processing before turn 2 starts.
    assert!(
        probe.wait_for_recorded_message("cjk-scrollback-marker", Duration::from_secs(30)),
        "first CJK turn must persist its assistant MessageEnd before turn 2: {}",
        probe.snapshot()
    );
    assert!(
        wait_for_idle_composer(&probe, Duration::from_secs(20)),
        "first CJK turn must settle and redraw an idle composer: {}",
        probe.snapshot()
    );

    // Second prompt forces enough settled overflow for the first turn to move
    // from the mutable live viewport into native scrollback.
    let busy_baseline = probe.count_plain("Working");
    probe.bracketed_paste("second-prompt-trigger");
    probe.send(b"\r");
    assert!(
        wait_for_busy_after(&probe, busy_baseline, Duration::from_secs(20)),
        "second turn must start streaming before it can settle: {}",
        probe.snapshot()
    );
    assert!(
        probe.wait_for_recorded_message("second-prompt-trigger", Duration::from_secs(20)),
        "second prompt must reach its canonical MessageEnd: {}",
        probe.snapshot()
    );
    assert!(
        wait_for_idle_composer(&probe, Duration::from_secs(20)),
        "second turn must settle so the first turn commits to scrollback: {}",
        probe.snapshot()
    );

    // The first user prompt must reach native scrollback with its CJK glyphs
    // contiguous (no leading space between 你学习下 and pi-coding-agent). The
    // live viewport paints the prompt through `Buffer::diff`, which already
    // skips continuation cells, so a contiguous match in the live paint is
    // not proof; `wait_for_committed_scrollback` inspects only the rows above
    // the live 24-row viewport, where the committed `insert_before` paint
    // lives. Before the WideCell fix the committed row reads
    // `你 学 习 下 pi-coding-agent` (a space between every glyph), so this
    // wait times out — the regression gate.
    assert!(
        wait_for_committed_scrollback(&probe, "你学习下pi-coding-agent", Duration::from_secs(20)),
        "first CJK/ASCII user prompt must commit to scrollback with contiguous CJK glyphs: {}",
        probe.snapshot()
    );
    assert!(
        wait_for_committed_scrollback(&probe, "┌─ mermaid · flowchart", Duration::from_secs(20)),
        "framed mermaid block must commit to scrollback: {}",
        probe.snapshot()
    );

    probe.send(&[CTRL_D]);
    assert!(
        probe.wait_for_count(SHOW_CURSOR, 1, Duration::from_secs(15)),
        "clean exit after CJK turns must restore the cursor: {}",
        probe.snapshot()
    );
    let status = probe
        .wait_exit(Duration::from_secs(15))
        .expect("rpi must exit after Ctrl+D");
    assert!(
        status.success(),
        "CJK scrollback exit must succeed: {status:?}"
    );

    // Replay the full PTY capture into a fixed-size screen plus unbounded
    // normal-screen scrollback (the repository's existing terminal model, now
    // wide-char aware so a committed CJK row reads each glyph once). The
    // committed scrollback is everything above the live 24-row viewport.
    let retained = replay_terminal_scrollback(&probe.snapshot(), 80, 24);
    let scrollback_end = retained.len().saturating_sub(24);
    eprintln!(
        "DIAG retained.len={} scrollback_end={}\n--- retained (idx: row) ---\n{}\n--- raw snapshot tail (last 400 chars) ---\n{}",
        retained.len(),
        scrollback_end,
        retained
            .iter()
            .enumerate()
            .map(|(i, line)| format!("{i:3}: {line:?}"))
            .collect::<Vec<_>>()
            .join("\n"),
        probe
            .snapshot()
            .chars()
            .rev()
            .take(400)
            .collect::<String>()
            .chars()
            .rev()
            .collect::<String>(),
    );
    let committed: Vec<&str> = retained[..scrollback_end]
        .iter()
        .map(|line| line.trim_end())
        .filter(|line| !line.is_empty())
        .collect();
    let find_first =
        |needle: &str| -> Option<usize> { committed.iter().position(|line| line.contains(needle)) };
    let find_last = |needle: &str| -> Option<usize> {
        committed.iter().rposition(|line| line.contains(needle))
    };

    // Exact ordered committed rows: the first user prompt, then the framed
    // assistant body and mermaid block. The raw PTY replay can retain one
    // pre-commit live paint of turn 2 above the final viewport when exit clears
    // it; use the last occurrence for ordering, then require exactly one copy
    // in the authoritative post-block committed suffix below.
    let user_row = find_first("你学习下pi-coding-agent")
        .expect("committed scrollback must contain the contiguous CJK/ASCII user prompt");
    let body_row = find_first("cjk-scrollback-marker")
        .expect("committed scrollback must contain the assistant body marker");
    let heading_row =
        find_first("framed section").expect("committed scrollback must contain the framed heading");
    let bullet_row =
        find_first("项目一").expect("committed scrollback must contain the CJK bullet 项目一");
    let mermaid_row = find_first("┌─ mermaid · flowchart")
        .expect("committed scrollback must contain the mermaid frame header");
    let second_row = find_last("second-prompt-trigger")
        .expect("committed scrollback must contain the second prompt");
    assert!(
        body_row > user_row,
        "assistant body must commit after the user prompt: {committed:#?}"
    );
    assert!(
        heading_row > body_row,
        "framed heading must follow the body marker line: {committed:#?}"
    );
    assert!(
        bullet_row > heading_row,
        "CJK bullets must follow the heading: {committed:#?}"
    );
    assert!(
        mermaid_row > bullet_row,
        "mermaid block must follow the markdown body: {committed:#?}"
    );
    assert!(
        second_row > mermaid_row,
        "second prompt must commit after the first turn's assistant block: {committed:#?}"
    );

    // The CJK assistant body glyphs must be contiguous in the committed rows
    // too (the same WideCell placeholder fix, exercised on assistant markdown
    // content rather than just the user card).
    assert!(
        committed.iter().any(|line| line.contains("你好世界")),
        "CJK marker 你好世界 must be contiguous in committed scrollback: {committed:#?}"
    );
    assert!(
        committed.iter().any(|line| line.contains("项目一")),
        "CJK bullet 项目一 must be contiguous in committed scrollback: {committed:#?}"
    );
    assert!(
        committed.iter().any(|line| line.contains("标题")),
        "CJK heading 标题 must be contiguous in committed scrollback: {committed:#?}"
    );

    // Exactly one copy of each settled prompt in its authoritative committed
    // region. `capture-pane -S -` includes history plus the current viewport;
    // the raw PTY replay likewise may retain an earlier live paint of turn 2.
    // Counting the suffix after turn 1's completed block excludes that capture
    // artifact while still failing on any actual duplicate commit.
    assert_eq!(
        committed[..body_row]
            .iter()
            .filter(|line| line.contains("你学习下pi-coding-agent"))
            .count(),
        1,
        "first user prompt must commit exactly once: {committed:#?}"
    );
    assert_eq!(
        committed[mermaid_row + 1..]
            .iter()
            .filter(|line| line.contains("second-prompt-trigger"))
            .count(),
        1,
        "second user prompt must commit exactly once after turn 1: {committed:#?}"
    );
    assert!(
        !committed
            .iter()
            .any(|line| line.contains("second-prompt-trigger")
                && line.contains("你学习下pi-coding-agent")),
        "no row may fuse two prompts (stale coordinate promotion): {committed:#?}"
    );

    // No stale composer / live frame promoted into committed scrollback.
    let stale_composer = committed.windows(3).any(|rows| {
        rows[0].contains("working")
            && rows[1].contains("╭── π")
            && rows[2].trim_start().starts_with("╰─")
    });
    assert!(
        !stale_composer,
        "standalone working status+composer frame leaked into scrollback: {committed:#?}"
    );
    assert!(
        !committed
            .iter()
            .any(|line| line.contains("╭── π") && line.contains("working")),
        "working composer header leaked into scrollback: {committed:#?}"
    );
    assert!(
        !committed
            .iter()
            .any(|line| line.trim_start().starts_with("╰─")),
        "empty composer footer leaked without its header into scrollback: {committed:#?}"
    );

    assert!(!probe.snapshot().contains(ENTER_ALT));
    assert!(probe.snapshot().contains(DISABLE_BRACKETED_PASTE));
    assert!(probe.snapshot().contains(ENABLE_LINE_WRAP));
    assert!(probe.snapshot().contains(SHOW_CURSOR));
}
#[test]
fn pty_mermaid_topology_sources_render_at_80_and_120_columns() {
    for cols in [80u16, 120] {
        let session_root = TempDir::new().expect("session root");
        let session_path = session_root.path().join("mermaid-topology-screen.jsonl");
        let session_path_arg = session_path.to_string_lossy().into_owned();
        let mut probe = PtyProbe::spawn_sized_with_seed(
            &[
                "--model",
                "faux/faux-1",
                "--session-dir",
                session_root.path().to_str().expect("session root utf8"),
                "--resume",
                session_path_arg.as_str(),
            ],
            &[],
            44,
            cols,
            |_, cwd| {
                seed_mermaid_topology_session(session_root.path(), cwd);
            },
        );
        assert!(
            await_entered(&probe),
            "TUI must resume Mermaid topology fixture"
        );
        let screen = probe
            .wait_for_live(Duration::from_secs(30), |live| {
                live.contains(MERMAID_EDGE_LABEL_NEEDLE)
                    && live.contains(MERMAID_DOTTED_LABEL_NEEDLE)
                    && live.contains(MERMAID_SEQUENCE_LABEL_NEEDLE)
            })
            .unwrap_or_else(|| {
                panic!(
                    "{cols} cols Mermaid screen missing: {:?}",
                    probe.live_screen()
                )
            });
        for needle in [
            MERMAID_SEQUENCE_LABEL_NEEDLE,
            MERMAID_EDGE_LABEL_NEEDLE,
            MERMAID_DOTTED_LABEL_NEEDLE,
        ] {
            assert!(
                screen.contains(needle),
                "{cols} cols missing {needle:?}: {screen}"
            );
        }
        assert!(!screen.contains("source fallback"), "{cols} cols: {screen}");
        assert!(!screen.contains("<br"), "{cols} cols: {screen}");
        assert!(
            !screen.lines().any(|line| line.trim() == "│ edges"),
            "{cols} cols: {screen}"
        );
        assert!(
            !screen.contains("\u{1b}["),
            "live screen must be ANSI-clean: {screen:?}"
        );
        for line in screen.lines() {
            let cells = line
                .chars()
                .map(|ch| UnicodeWidthChar::width(ch).unwrap_or(0))
                .sum::<usize>();
            assert!(
                cells <= usize::from(cols),
                "{cols} cols overflow ({cells}): {line:?}"
            );
        }

        probe.send(&[CTRL_D]);
        let status = probe
            .wait_exit(Duration::from_secs(15))
            .expect("rpi Mermaid fixture exit");
        assert!(
            status.success(),
            "Mermaid fixture must exit cleanly: {status:?}"
        );
        let plain = strip_ansi(&probe.snapshot());
        assert!(
            plain.contains(MERMAID_QUOTED_LABEL_NEEDLE),
            "{cols} cols: {plain}"
        );
        assert!(
            plain.contains(MERMAID_STATE_LABEL_NEEDLE),
            "{cols} cols: {plain}"
        );
        assert!(
            plain.contains(MERMAID_SEQUENCE_LABEL_NEEDLE),
            "{cols} cols: {plain}"
        );
    }
}

#[test]
fn pty_mermaid_js_rejected_slash_source_falls_back_at_80_and_120_columns() {
    for cols in [80u16, 120] {
        let session_root = TempDir::new().expect("session root");
        let session_path = session_root.path().join("mermaid-slash-fallback.jsonl");
        let session_path_arg = session_path.to_string_lossy().into_owned();
        let mut probe = PtyProbe::spawn_sized_with_seed(
            &[
                "--model",
                "faux/faux-1",
                "--session-dir",
                session_root.path().to_str().expect("session root utf8"),
                "--resume",
                session_path_arg.as_str(),
            ],
            &[],
            32,
            cols,
            |_, cwd| {
                use pi_ai::{AssistantMessage, ContentBlock, Message, Model, StopReason};
                let mut model = Model::default();
                model.id = "faux-1".to_owned();
                model.api = "faux".into();
                model.provider = "faux".into();
                let recorder = pi_coding::start_session_in(
                    cwd,
                    Some(&model),
                    Some("off"),
                    Some(session_root.path()),
                    Some("mermaid-slash-fallback"),
                    None,
                )
                .expect("start slash fallback session");
                recorder
                    .record_message(&Message::user_text("render rejected slash source", 1))
                    .expect("record slash prompt");
                let mut assistant = AssistantMessage::pending(&model);
                assistant.content = vec![ContentBlock::text(
                    "```mermaid\nflowchart LR\nbash --> brush / ProcessManager / sandbox\n```",
                )];
                assistant.stop_reason = StopReason::Stop;
                assistant.timestamp = 2;
                recorder
                    .record_message(&Message::Assistant(assistant))
                    .expect("record slash response");
                recorder.persist_now().expect("persist slash session");
                recorder.close().expect("close slash session");
            },
        );
        assert!(await_entered(&probe), "TUI must resume slash fixture");
        let screen = probe
            .wait_for_live(Duration::from_secs(30), |live| {
                live.contains("source fallback")
                    && live.contains("brush / ProcessManager / sandbox")
            })
            .unwrap_or_else(|| panic!("{cols} cols slash fallback missing: {}", probe.live_screen()));
        assert!(screen.contains("Unsupported flowchart syntax"), "{screen}");
        assert!(!screen.contains("▶brush"), "must not invent slash targets: {screen}");
        assert!(!screen.contains("\u{1b}["), "live screen must be ANSI-clean: {screen:?}");
        probe.send(&[CTRL_D]);
        let status = probe
            .wait_exit(Duration::from_secs(15))
            .expect("rpi slash fixture exit");
        assert!(status.success(), "slash fixture must exit cleanly: {status:?}");
    }
}

#[test]
fn pty_restored_history_scroll_keeps_tool_card_mermaid_and_cjk_reachable() {
    let session_root = TempDir::new().expect("session root");
    let session_path = session_root
        .path()
        .join(format!("{RESTORED_SCROLL_SESSION_ID}.jsonl"));
    let session_path_arg = session_path.to_string_lossy().into_owned();
    let mut probe = PtyProbe::spawn_sized_with_seed(
        &[
            "--model",
            "faux/faux-1",
            "--session-dir",
            session_root.path().to_str().expect("session root utf8"),
            "--resume",
            session_path_arg.as_str(),
        ],
        &[],
        30,
        100,
        |_, cwd| {
            seed_restored_scroll_session(session_root.path(), cwd);
        },
    );
    assert!(
        await_entered(&probe),
        "TUI must resume the restored fixture"
    );
    let tail_screen = probe
        .wait_for_live(Duration::from_secs(30), |live| {
            live.contains(TAIL_SCROLL_SENTINEL)
        })
        .unwrap_or_else(|| {
            let live = probe.live_screen();
            panic!(
                "restored tail must be visible in follow mode; running={} live={live:?}",
                probe.is_running()
            )
        });
    assert!(!tail_screen.contains(OLDEST_SCROLL_SENTINEL));
    assert!(probe.is_running(), "rpi must survive restored hydration");

    let mut saw_tool_card = false;
    let mut saw_flowchart = false;
    let mut saw_class = false;
    let mut saw_mermaid = false;
    let mut saw_state = false;
    let mut saw_er = false;
    let mut saw_oldest = false;
    for _ in 0..128 {
        let before = probe.snapshot().len();
        probe.page_up();
        assert!(
            probe.wait_for_output_growth(before, Duration::from_secs(2)),
            "PageUp must trigger a repaint"
        );
        let live = probe.live_screen();
        if !saw_tool_card && live.contains(TOOL_CARD_TITLE_SENTINEL) {
            assert_eq!(live.matches(TOOL_CARD_TITLE_SENTINEL).count(), 1);
            assert!(
                live.contains("line 3"),
                "compact read title viewport must expose the first retained tail row"
            );
            assert!(
                !live.contains("\u{1b}[31m") && !live.contains("\u{1b}[0m"),
                "compact ToolCard must not expose raw ANSI controls"
            );

            let mut saw_compact_last_row = live.contains("line 8");
            let mut saw_compact_fold = live.contains("… 2 more lines ⟦Ctrl+O: Expand⟧");
            let mut saw_compact_mid_row = live.contains("line 5");
            let mut compact_tail = live.clone();
            for _ in 0..8 {
                if saw_compact_last_row && saw_compact_fold && saw_compact_mid_row {
                    break;
                }
                let before_tail = probe.snapshot().len();
                probe.page_down();
                assert!(
                    probe.wait_for_output_growth(before_tail, Duration::from_secs(2)),
                    "PageDown must expose the compact ToolCard tail"
                );
                compact_tail = probe.live_screen();
                saw_compact_last_row |= compact_tail.contains("line 8");
                saw_compact_fold |= compact_tail.contains("… 2 more lines ⟦Ctrl+O: Expand⟧");
                saw_compact_mid_row |= compact_tail.contains("line 5");
            }
            assert!(
                saw_compact_last_row,
                "compact ToolCard final row must remain reachable"
            );
            assert!(
                saw_compact_fold,
                "compact ToolCard production fold must remain reachable"
            );
            assert!(
                saw_compact_mid_row,
                "compact ToolCard retained rows must remain reachable"
            );
            assert!(!compact_tail.contains(TOOL_CARD_OUTPUT_SENTINEL));

            let before_title = probe.snapshot().len();
            probe.page_up();
            assert!(
                probe.wait_for_output_growth(before_title, Duration::from_secs(2)),
                "PageUp must return to the compact ToolCard title"
            );
            probe
                .wait_for_live(Duration::from_secs(2), |screen| {
                    screen.contains(TOOL_CARD_TITLE_SENTINEL)
                })
                .unwrap_or_else(|| {
                    panic!(
                        "compact ToolCard title must remain reachable before expansion; running={} live={:?}",
                        probe.is_running(),
                        probe.live_screen()
                    )
                });

            let before_expand = probe.snapshot().len();
            probe.send(&[CTRL_O]);
            assert!(
                probe.wait_for_output_growth(before_expand, Duration::from_secs(2)),
                "Ctrl+O must repaint the ToolCard in expanded mode"
            );
            let expanded = probe
                .wait_for_live(Duration::from_secs(2), |screen| {
                    screen.contains(TOOL_CARD_TITLE_SENTINEL)
                        && screen.contains(TOOL_CARD_OUTPUT_SENTINEL)
                })
                .unwrap_or_else(|| {
                    panic!(
                        "expanded ToolCard must expose its folded first row; running={} live={:?}",
                        probe.is_running(),
                        probe.live_screen()
                    )
                });
            assert_eq!(expanded.matches(TOOL_CARD_TITLE_SENTINEL).count(), 1);
            assert_eq!(expanded.matches(TOOL_CARD_OUTPUT_SENTINEL).count(), 1);
            assert!(!expanded.contains("Ctrl+O: Expand"));
            assert!(
                !expanded.contains("\u{1b}[31m") && !expanded.contains("\u{1b}[0m"),
                "expanded ToolCard CJK output must be ANSI-clean"
            );
            saw_tool_card = true;
        }
        if !saw_flowchart && live.contains(FLOWCHART_SCROLL_SENTINEL) {
            assert!(live.contains("mermaid · flowchart"));
            assert!(!live.contains("source fallback"));
            let mut body = live;
            for _ in 0..8 {
                if body.contains(FLOWCHART_CJK_NEEDLE) {
                    break;
                }
                let before_body = probe.snapshot().len();
                probe.page_down();
                assert!(probe.wait_for_output_growth(before_body, Duration::from_secs(2)));
                body = probe.live_screen();
            }
            assert!(body.contains(FLOWCHART_CJK_NEEDLE));
            assert!(!body.contains("source fallback"));
            assert!(!body.contains("\u{1b}[31m") && !body.contains("\u{1b}[0m"));
            saw_flowchart = true;
            continue;
        }
        if !saw_class && live.contains(CLASS_SCROLL_SENTINEL) {
            assert!(live.contains("mermaid · classDiagram"));
            assert!(!live.contains("source fallback"));
            let mut body = live;
            for _ in 0..8 {
                if body.contains(CLASS_CJK_NEEDLE) {
                    break;
                }
                let before_body = probe.snapshot().len();
                probe.page_down();
                assert!(probe.wait_for_output_growth(before_body, Duration::from_secs(2)));
                body = probe.live_screen();
            }
            assert!(body.contains(CLASS_CJK_NEEDLE));
            assert!(!body.contains("source fallback"));
            assert!(!body.contains("\u{1b}[31m") && !body.contains("\u{1b}[0m"));
            saw_class = true;
            continue;
        }
        if live.contains(MERMAID_SCROLL_SENTINEL) {
            saw_mermaid = true;
            assert!(live.contains("mermaid · sequenceDiagram"));
            assert!(live.contains(MERMAID_CJK_SEQUENCE_SENTINEL));
            assert!(!live.contains("source fallback"));
            assert!(
                !live.contains("\u{1b}[31m") && !live.contains("\u{1b}[0m"),
                "sequence card must be ANSI-clean"
            );
            assert!(
                live.lines().any(|line| {
                    !line.contains("mermaid ·") && line.contains(MERMAID_CJK_SEQUENCE_SENTINEL)
                }),
                "sequence card must render nonblank body content with its CJK needle"
            );
        }
        if !saw_state && live.contains(STATE_SCROLL_SENTINEL) {
            assert!(live.contains("mermaid · stateDiagram"));
            assert!(!live.contains("source fallback"));
            assert!(!live.contains("Unsupported stateDiagram syntax"));
            assert!(!live.contains("<br"));
            assert!(
                !live.contains("\u{1b}[31m") && !live.contains("\u{1b}[0m"),
                "state card title must be ANSI-clean"
            );
            let mut state_body = live;
            for _ in 0..8 {
                if state_body.contains(STATE_CJK_NEEDLE) {
                    break;
                }
                let before_state = probe.snapshot().len();
                probe.page_down();
                assert!(probe.wait_for_output_growth(before_state, Duration::from_secs(2)));
                state_body = probe.live_screen();
            }
            assert!(state_body.contains(STATE_CJK_NEEDLE));
            assert!(state_body.contains("is_error=true"));
            assert!(!state_body.contains("source fallback"));
            assert!(!state_body.contains("Unsupported stateDiagram syntax"));
            assert!(!state_body.contains("<br"));
            assert!(
                !state_body.contains("\u{1b}[31m") && !state_body.contains("\u{1b}[0m"),
                "state card body must be ANSI-clean"
            );
            assert!(
                state_body
                    .lines()
                    .any(|line| line.contains(STATE_CJK_NEEDLE)),
                "state card must render nonblank body content with its CJK needle"
            );
            saw_state = true;
            continue;
        }
        if !saw_er && live.contains(ER_SCROLL_SENTINEL) {
            assert!(live.contains("mermaid · erDiagram"));
            assert!(!live.contains("source fallback"));
            assert!(
                !live.contains("\u{1b}[31m") && !live.contains("\u{1b}[0m"),
                "ER card title must be ANSI-clean"
            );
            let mut er_body = live;
            for _ in 0..8 {
                if er_body.contains(ER_CJK_NEEDLE) {
                    break;
                }
                let before_er = probe.snapshot().len();
                probe.page_down();
                assert!(probe.wait_for_output_growth(before_er, Duration::from_secs(2)));
                er_body = probe.live_screen();
            }
            assert!(er_body.contains(ER_CJK_NEEDLE));
            assert!(!er_body.contains("source fallback"));
            assert!(
                !er_body.contains("\u{1b}[31m") && !er_body.contains("\u{1b}[0m"),
                "ER card body must be ANSI-clean"
            );
            assert!(
                er_body.lines().any(|line| line.contains(ER_CJK_NEEDLE)),
                "ER card must render nonblank body content with its CJK needle"
            );
            saw_er = true;
            continue;
        }
        saw_oldest |= live.contains(OLDEST_SCROLL_SENTINEL);
        if saw_tool_card
            && saw_flowchart
            && saw_class
            && saw_mermaid
            && saw_state
            && saw_er
            && saw_oldest
        {
            break;
        }
        assert!(probe.is_running(), "rpi must survive repeated PageUp");
    }
    assert!(
        saw_tool_card,
        "PageUp must reach the strict succeeded ToolCard title"
    );
    assert!(
        saw_flowchart,
        "PageUp must reach the exact CJK Mermaid flowchart"
    );
    assert!(saw_class, "PageUp must reach the exact CJK Mermaid class");
    assert!(
        saw_mermaid,
        "PageUp must reach the exact CJK Mermaid sequence"
    );
    assert!(saw_state, "PageUp must reach the exact CJK Mermaid state");
    assert!(saw_er, "PageUp must reach the exact CJK Mermaid ER");
    assert!(saw_oldest, "PageUp must reach the oldest restored entry");

    probe.resize(22, 74);
    assert!(
        probe.is_running(),
        "rpi must survive 100x30 to 74x22 resize"
    );
    let mut saw_tool_title_after_resize = false;
    let mut saw_tool_output_after_resize = false;
    let mut restored_compact_after_resize = false;
    let mut saw_flowchart_after_resize = false;
    let mut saw_class_after_resize = false;
    let mut saw_mermaid_after_resize = false;
    let mut saw_state_after_resize = false;
    let mut saw_er_after_resize = false;
    for _ in 0..128 {
        let live = probe.live_screen();
        if !restored_compact_after_resize
            && live.contains(TOOL_CARD_TITLE_SENTINEL)
            && live.contains(TOOL_CARD_OUTPUT_SENTINEL)
        {
            saw_tool_title_after_resize = true;
            assert!(
                live.contains(TOOL_CARD_OUTPUT_SENTINEL),
                "expanded ToolCard must keep its first CJK row visible after resize"
            );
            saw_tool_output_after_resize = true;
            assert!(
                !live.contains("\u{1b}[31m") && !live.contains("\u{1b}[0m"),
                "resized expanded ToolCard CJK output must remain ANSI-clean"
            );

            let before_compact = probe.snapshot().len();
            probe.send(&[CTRL_O]);
            assert!(
                probe.wait_for_output_growth(before_compact, Duration::from_secs(2)),
                "Ctrl+O must repaint the resized ToolCard in compact mode"
            );
            let compact_title = probe
                .wait_for_live(Duration::from_secs(2), |screen| {
                    screen.contains(TOOL_CARD_TITLE_SENTINEL)
                        && screen.contains("line 3")
                        && !screen.contains(TOOL_CARD_OUTPUT_SENTINEL)
                })
                .unwrap_or_else(|| {
                    panic!(
                        "resized ToolCard must return to its compact title rows; running={} live={:?}",
                        probe.is_running(),
                        probe.live_screen()
                    )
                });
            assert_eq!(compact_title.matches(TOOL_CARD_TITLE_SENTINEL).count(), 1);

            let mut compact_tail = compact_title;
            let mut saw_compact_last_row = compact_tail.contains("line 8");
            let mut saw_compact_fold = compact_tail.contains("… 2 more lines ⟦Ctrl+O: Expand⟧");
            for _ in 0..8 {
                if saw_compact_last_row && saw_compact_fold {
                    break;
                }
                let before_tail = probe.snapshot().len();
                probe.page_down();
                assert!(probe.wait_for_output_growth(before_tail, Duration::from_secs(2)));
                compact_tail = probe.live_screen();
                saw_compact_last_row |= compact_tail.contains("line 8");
                saw_compact_fold |= compact_tail.contains("… 2 more lines ⟦Ctrl+O: Expand⟧");
            }
            assert!(
                saw_compact_last_row,
                "resized compact ToolCard final row must remain reachable"
            );
            assert!(
                saw_compact_fold,
                "resized compact ToolCard fold must remain reachable"
            );
            assert!(!compact_tail.contains(TOOL_CARD_OUTPUT_SENTINEL));
            restored_compact_after_resize = true;
            continue;
        }
        if !saw_flowchart_after_resize && live.contains(FLOWCHART_SCROLL_SENTINEL) {
            assert!(!live.contains("source fallback"));
            let mut body = live;
            for _ in 0..8 {
                if body.contains(FLOWCHART_CJK_NEEDLE) {
                    break;
                }
                let before_body = probe.snapshot().len();
                probe.page_down();
                assert!(probe.wait_for_output_growth(before_body, Duration::from_secs(2)));
                body = probe.live_screen();
            }
            assert!(body.contains(FLOWCHART_CJK_NEEDLE));
            assert!(!body.contains("source fallback"));
            assert!(!body.contains("\u{1b}[31m") && !body.contains("\u{1b}[0m"));
            saw_flowchart_after_resize = true;
            continue;
        }
        if !saw_class_after_resize && live.contains(CLASS_SCROLL_SENTINEL) {
            assert!(!live.contains("source fallback"));
            let mut body = live;
            for _ in 0..8 {
                if body.contains(CLASS_CJK_NEEDLE) {
                    break;
                }
                let before_body = probe.snapshot().len();
                probe.page_down();
                assert!(probe.wait_for_output_growth(before_body, Duration::from_secs(2)));
                body = probe.live_screen();
            }
            assert!(body.contains(CLASS_CJK_NEEDLE));
            assert!(!body.contains("source fallback"));
            assert!(!body.contains("\u{1b}[31m") && !body.contains("\u{1b}[0m"));
            saw_class_after_resize = true;
            continue;
        }
        if !saw_mermaid_after_resize && live.contains(MERMAID_SCROLL_SENTINEL) {
            assert!(live.contains("mermaid · sequenceDiagram"));
            assert!(!live.contains("source fallback"));
            let before_body = probe.snapshot().len();
            probe.page_down();
            assert!(probe.wait_for_output_growth(before_body, Duration::from_secs(2)));
            let body = probe.live_screen();
            assert!(body.contains(MERMAID_CJK_SEQUENCE_SENTINEL));
            assert!(!body.contains("source fallback"));
            assert!(!body.contains("\u{1b}[31m") && !body.contains("\u{1b}[0m"));
            saw_mermaid_after_resize = true;
            continue;
        }
        if !saw_state_after_resize && live.contains(STATE_SCROLL_SENTINEL) {
            assert!(!live.contains("source fallback"));
            assert!(!live.contains("Unsupported stateDiagram syntax"));
            assert!(!live.contains("<br"));
            let mut body = live;
            for _ in 0..8 {
                if body.contains(STATE_CJK_NEEDLE) {
                    break;
                }
                let before_body = probe.snapshot().len();
                probe.page_down();
                assert!(probe.wait_for_output_growth(before_body, Duration::from_secs(2)));
                body = probe.live_screen();
            }
            assert!(body.contains(STATE_CJK_NEEDLE));
            assert!(body.contains("is_error=true"));
            assert!(!body.contains("source fallback"));
            assert!(!body.contains("Unsupported stateDiagram syntax"));
            assert!(!body.contains("\u{1b}[31m") && !body.contains("\u{1b}[0m"));
            assert!(!body.contains("<br"));
            saw_state_after_resize = true;
            continue;
        }
        if !saw_er_after_resize && live.contains(ER_SCROLL_SENTINEL) {
            assert!(!live.contains("source fallback"));
            let mut body = live;
            for _ in 0..8 {
                if body.contains(ER_CJK_NEEDLE) {
                    break;
                }
                let before_body = probe.snapshot().len();
                probe.page_down();
                assert!(probe.wait_for_output_growth(before_body, Duration::from_secs(2)));
                body = probe.live_screen();
            }
            assert!(body.contains(ER_CJK_NEEDLE));
            assert!(!body.contains("source fallback"));
            assert!(!body.contains("\u{1b}[31m") && !body.contains("\u{1b}[0m"));
            saw_er_after_resize = true;
            continue;
        }
        if saw_tool_title_after_resize
            && saw_tool_output_after_resize
            && restored_compact_after_resize
            && saw_flowchart_after_resize
            && saw_class_after_resize
            && saw_mermaid_after_resize
            && saw_state_after_resize
            && saw_er_after_resize
        {
            break;
        }
        let before = probe.snapshot().len();
        probe.page_down();
        assert!(probe.wait_for_output_growth(before, Duration::from_secs(2)));
        assert!(probe.is_running(), "rpi must survive PageDown after resize");
    }
    assert!(
        saw_tool_title_after_resize,
        "strict succeeded ToolCard title must remain reachable after resize"
    );
    assert!(
        saw_tool_output_after_resize,
        "expanded ToolCard CJK output must remain reachable after resize"
    );
    assert!(
        restored_compact_after_resize,
        "Ctrl+O must restore compact ToolCard rows after resize"
    );
    assert!(
        saw_flowchart_after_resize,
        "CJK Mermaid flowchart must remain reachable after resize"
    );
    assert!(
        saw_class_after_resize,
        "CJK Mermaid class must remain reachable after resize"
    );
    assert!(
        saw_mermaid_after_resize,
        "CJK Mermaid sequence must remain reachable after resize"
    );
    assert!(
        saw_state_after_resize,
        "CJK Mermaid state must remain reachable after resize"
    );
    assert!(
        saw_er_after_resize,
        "CJK Mermaid ER must remain reachable after resize"
    );

    let mut saw_tail_after_resize = false;
    for _ in 0..128 {
        let live = probe.live_screen();
        if live.contains(TAIL_SCROLL_SENTINEL) {
            saw_tail_after_resize = true;
            break;
        }
        let before = probe.snapshot().len();
        probe.page_down();
        assert!(probe.wait_for_output_growth(before, Duration::from_secs(2)));
        assert!(
            probe.is_running(),
            "rpi must survive PageDown to restored tail"
        );
    }
    assert!(
        saw_tail_after_resize,
        "PageDown must return to the restored tail after resize"
    );

    probe.send(&[CTRL_D]);
    let status = probe
        .wait_exit(Duration::from_secs(15))
        .expect("rpi must exit after restored scroll regression");
    assert!(
        status.success(),
        "restored scroll regression must exit cleanly: {status:?}"
    );

    let retained = replay_terminal_scrollback(&probe.snapshot(), 74, 22);
    let retained_text = retained.join("\n");
    assert_eq!(
        retained_text.matches(TOOL_CARD_TITLE_SENTINEL).count(),
        1,
        "restored compact ToolCard must enter retained history exactly once"
    );
    assert!(retained_text.contains(OLDEST_SCROLL_SENTINEL));
    assert!(retained_text.contains(FLOWCHART_CJK_NEEDLE));
    assert!(retained_text.contains(CLASS_CJK_NEEDLE));
    assert!(retained_text.contains(MERMAID_CJK_SEQUENCE_SENTINEL));
    assert!(retained_text.contains(STATE_CJK_NEEDLE));
    assert!(retained_text.contains(ER_CJK_NEEDLE));
    assert!(retained_text.contains(TAIL_SCROLL_SENTINEL));
    assert!(
        !retained_text
            .lines()
            .any(|line| line.contains("working") && line.contains("╭── π"))
    );
    assert!(!probe.snapshot().contains(ENTER_ALT));
    assert!(probe.snapshot().contains(SHOW_CURSOR));
}

#[test]
fn pty_initialization_error_leaves_terminal_clean() {
    // No --model and no auth (cleared env) => build_session fails before the
    // TUI is ever entered, so no terminal-acquisition escape is emitted.
    let mut probe = PtyProbe::spawn(&[], &[]);
    assert!(
        probe.wait_for_count(
            "No authenticated models available",
            1,
            Duration::from_secs(30)
        ),
        "init error must report missing auth: {}",
        probe.snapshot()
    );
    let status = probe
        .wait_exit(Duration::from_secs(15))
        .expect("rpi must exit on initialization error");
    assert!(
        !status.success(),
        "init error must exit non-zero: {status:?}"
    );
    let out = probe.snapshot();
    assert!(
        !out.contains(ENTER_ALT),
        "init error must not enter the alternate screen: {out}"
    );
}

#[test]
fn pty_panic_restores_terminal() {
    let mut probe = PtyProbe::spawn(
        &["--model", "faux/faux-1"],
        &[("PI_TEST_PANIC_AFTER_ENTER", "1")],
    );
    assert!(
        await_acquired(&probe),
        "TUI must acquire the cursor before panicking: {}",
        probe.snapshot()
    );
    assert!(!probe.snapshot().contains(ENTER_ALT));
    assert!(
        probe.wait_for_count(SHOW_CURSOR, 1, Duration::from_secs(15)),
        "panic hook must restore the cursor: {}",
        probe.snapshot()
    );
    let status = probe
        .wait_exit(Duration::from_secs(15))
        .expect("rpi must exit after panic");
    assert_eq!(
        status.code(),
        Some(101),
        "uncaught panic must exit with code 101: {status:?}"
    );
    let out = probe.snapshot();
    // The panic report must be written BEFORE the terminal restore: the
    // restore emits a tmux passthrough DCS that leaves tmux's screen parser
    // unable to render anything written after it, so a report printed after
    // the restore is invisible in a tmux pane (silent exit 101). The report
    // must also carry the payload and a backtrace, not just the restore.
    let message_at = out.find("pi-test: panic after TUI enter");
    let backtrace_at = out.find("stack backtrace:");
    let show_at = out.find(SHOW_CURSOR);
    assert!(
        matches!((message_at, backtrace_at, show_at), (Some(m), Some(b), Some(s)) if m < s && b < s),
        "panic report (message + backtrace) must precede the terminal restore in the pty stream: {out}"
    );
}

#[test]
fn pty_sigterm_restores_terminal_and_exits_cleanly() {
    let mut probe = PtyProbe::spawn(&["--model", "faux/faux-1"], &[]);
    assert!(
        await_entered(&probe),
        "TUI must acquire inline viewport: {}",
        probe.snapshot()
    );
    assert!(!probe.snapshot().contains(ENTER_ALT));
    thread::sleep(Duration::from_millis(200));
    probe.signal(Signal::SIGTERM);
    assert!(
        probe.wait_for_count(SHOW_CURSOR, 1, Duration::from_secs(15)),
        "SIGTERM must restore the cursor: {}",
        probe.snapshot()
    );
    let status = probe
        .wait_exit(Duration::from_secs(15))
        .expect("rpi must exit after SIGTERM");
    assert!(
        status.success(),
        "SIGTERM is handled gracefully (exit 0): {status:?}"
    );
}

#[test]
fn pty_sighup_restores_terminal_and_exits_cleanly() {
    let mut probe = PtyProbe::spawn(&["--model", "faux/faux-1"], &[]);
    assert!(
        await_entered(&probe),
        "TUI must acquire inline viewport: {}",
        probe.snapshot()
    );
    assert!(!probe.snapshot().contains(ENTER_ALT));
    thread::sleep(Duration::from_millis(200));
    probe.signal(Signal::SIGHUP);
    assert!(
        probe.wait_for_count(SHOW_CURSOR, 1, Duration::from_secs(15)),
        "SIGHUP must restore the cursor: {}",
        probe.snapshot()
    );
    let status = probe
        .wait_exit(Duration::from_secs(15))
        .expect("rpi must exit after SIGHUP");
    assert!(
        status.success(),
        "SIGHUP is handled gracefully (exit 0): {status:?}"
    );
}

#[test]
fn pty_suspend_resume_yields_and_reacquires_terminal() {
    let mut probe = PtyProbe::spawn(&["--model", "faux/faux-1"], &[]);
    assert!(
        await_entered(&probe),
        "TUI must acquire inline viewport: {}",
        probe.snapshot()
    );
    assert!(!probe.snapshot().contains(ENTER_ALT));
    probe.send(b"/logout anthropic\r");
    assert!(
        probe.wait_for_count(SHOW_CURSOR, 1, Duration::from_secs(15)),
        "suspend must restore the cursor: {}",
        probe.snapshot()
    );
    assert!(
        probe.wait_for_count(HIDE_CURSOR, 2, Duration::from_secs(15)),
        "resume must reacquire the cursor: {}",
        probe.snapshot()
    );
    probe.send(&[CTRL_D]);
    assert!(
        probe.wait_for_count(SHOW_CURSOR, 2, Duration::from_secs(15)),
        "exit after resume must restore the cursor again: {}",
        probe.snapshot()
    );
    let status = probe
        .wait_exit(Duration::from_secs(15))
        .expect("rpi must exit after resume + Ctrl+D");
    assert!(
        status.success(),
        "suspend/resume then quit must exit 0: {status:?}"
    );
}

#[test]
fn pty_double_ctrl_c_exits_and_restores_terminal() {
    let mut probe = PtyProbe::spawn(&["--model", "faux/faux-1"], &[]);
    assert!(
        await_entered(&probe),
        "TUI must acquire the inline viewport: {}",
        probe.snapshot()
    );
    assert!(!probe.snapshot().contains(ENTER_ALT));

    // First idle Ctrl-C arms the exit ladder only.
    probe.send(&[CTRL_C]);
    thread::sleep(Duration::from_millis(80));
    assert!(
        probe.child.try_wait().ok().flatten().is_none(),
        "single Ctrl-C must not exit: {}",
        probe.snapshot()
    );

    // Second press within 500ms exits through the normal return path.
    probe.send(&[CTRL_C]);
    assert!(
        probe.wait_for_count(DISABLE_BRACKETED_PASTE, 1, Duration::from_secs(15)),
        "double Ctrl-C must disable bracketed paste: {}",
        probe.snapshot()
    );
    assert!(
        probe.snapshot().contains(SHOW_CURSOR),
        "double Ctrl-C exit must restore the cursor: {}",
        probe.snapshot()
    );
    let status = probe
        .wait_exit(Duration::from_secs(15))
        .expect("rpi must exit after double Ctrl-C");
    assert!(
        status.success(),
        "double Ctrl-C clean exit must report success: {status:?}"
    );
    assert!(
        probe.snapshot().contains(CLEAR_AFTER_CURSOR),
        "double Ctrl-C exit must clear only the live inline viewport"
    );
}

#[test]
fn pty_single_ctrl_c_does_not_exit() {
    let mut probe = PtyProbe::spawn(&["--model", "faux/faux-1"], &[]);
    assert!(
        await_entered(&probe),
        "TUI must acquire the inline viewport: {}",
        probe.snapshot()
    );

    probe.send(&[CTRL_C]);
    // Wait past the 500ms double-press window.
    thread::sleep(Duration::from_millis(700));
    assert!(
        probe.child.try_wait().ok().flatten().is_none(),
        "single Ctrl-C must leave the TUI running: {}",
        probe.snapshot()
    );

    // Ctrl-D remains the direct empty-editor exit.
    probe.send(&[CTRL_D]);
    assert!(
        probe.wait_for_count(SHOW_CURSOR, 1, Duration::from_secs(15)),
        "Ctrl+D after single Ctrl-C must still restore the cursor: {}",
        probe.snapshot()
    );
    let status = probe
        .wait_exit(Duration::from_secs(15))
        .expect("rpi must exit after Ctrl+D");
    assert!(
        status.success(),
        "Ctrl+D exit after single Ctrl-C must succeed: {status:?}"
    );
}
/// Esc byte (0x1b) in raw mode. The TUI keybindings map Esc to `Action::Abort`,
/// which cancels a loop-owned active turn via `Application::abort`.
const ESC: u8 = 0x1b;

/// Contract: Esc during a `/loop`-owned active iteration cancels the active
/// iteration, settles loop state without a failure toast ("Loop <id> failed:
/// Request was aborted"), leaves the composer usable, and restores the
/// terminal on exit. The loop remains scheduled (only the iteration is
/// cancelled).
///
/// Plausible bug: the abort only cancelled the session, not the loop iteration
/// token, so the scheduler misclassified the expected user Esc as a provider
/// failure and raised the red error toast.
#[test]
fn pty_loop_iteration_esc_aborts_without_failure_toast() {
    // A long faux response keeps the loop iteration streaming so Esc lands
    // mid-flight instead of racing an instantaneous completion.
    let long_response = "loopescstream-".repeat(1500);
    let mut probe = PtyProbe::spawn(
        &["--model", "faux/faux-1"],
        &[("PI_FAUX_RESPONSE", long_response.as_str())],
    );
    assert!(
        await_entered(&probe),
        "TUI must acquire the inline viewport: {}",
        probe.snapshot()
    );
    assert!(
        !probe.snapshot().contains(ENTER_ALT),
        "loop path must stay on the normal screen"
    );

    // Schedule a loop that fires immediately and streams the long faux reply.
    probe.send(b"/loop 30m check status\r");
    assert!(
        probe.wait_for_count("loopescstream", 1, Duration::from_secs(20)),
        "loop iteration must start streaming the faux reply: {}",
        probe.snapshot()
    );

    probe.send(&[ESC]);
    // The abort must clear the active `working` UI and restore the idle
    // composer: the OMP-style status line above the header carries the
    // spinner/`working` text while streaming and is blank once the turn
    // settles. Assert this directly — not via the transient "Aborting"
    // status text, which is only rendered while NOT streaming and therefore
    // never appears for an in-flight cancellation.
    assert!(
        wait_for_idle_composer(&probe, Duration::from_secs(10)),
        "abort must clear the working UI and restore the idle composer: {}",
        probe.snapshot()
    );
    // No failure toast for the expected user abort.
    let after = probe.snapshot();
    assert!(
        !after.contains("failed: Request was aborted"),
        "user Esc must not raise a Loop failed toast: {after}"
    );

    // Composer remains usable: pasted input renders after the abort. Bracketed
    // paste → one Event::Paste → one contiguous render; per-key typing
    // interleaves cursor/separator cells between chars in the raw stream.
    probe.bracketed_paste("composerstillusable");
    assert!(
        probe.wait_for_count("composerstillusable", 1, Duration::from_secs(5)),
        "composer must remain usable after loop iteration abort: {}",
        probe.snapshot()
    );
    // Ctrl+D is the direct quit only with an empty editor; clear the pasted
    probe.send(&[CTRL_C]);
    thread::sleep(Duration::from_millis(150));

    // Clean exit restores the terminal.
    probe.send(&[CTRL_D]);
    assert!(
        probe.wait_for_count(SHOW_CURSOR, 1, Duration::from_secs(15)),
        "clean exit after loop Esc must restore the cursor: {}",
        probe.snapshot()
    );
    let status = probe
        .wait_exit(Duration::from_secs(15))
        .expect("rpi must exit after Ctrl+D");
    assert!(
        status.success(),
        "loop Esc exit must report success: {status:?}"
    );
    assert!(
        probe.snapshot().contains(CLEAR_AFTER_CURSOR),
        "loop Esc exit must clear only the live inline viewport"
    );
}
