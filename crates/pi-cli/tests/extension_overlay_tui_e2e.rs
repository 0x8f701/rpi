//! Real-binary PTY coverage for the extension overlay surface: a trusted
//! QuickJS extension registers an overlay (pi.registerOverlay) and the
//! `/overlay <id>` slash command opens the rendered panel. This closes the
//! e2e gap left by the in-process runtime tests (crates/pi-coding/tests/
//! extensions.rs overlay_* suite) and the TUI unit renderers (tui.rs
//! extension_overlay_*): the full binary path from slash command dispatch
//! through `Application::extension_runtime().overlays()` and
//! `invoke_overlay_render` to the rendered panel rows.
#![cfg(unix)]

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use nix::pty::{openpty, Winsize};
use nix::sys::termios::Termios;
use tempfile::TempDir;

fn rpi_bin() -> String {
    env!("CARGO_BIN_EXE_rpi").to_owned()
}

/// Trusted QuickJS extension that registers one overlay and renders three
/// rows (plain, styled-accent, styled-error). Mirrors the
/// `load_quickjs_overlay_fixture` fixture shape from
/// crates/pi-coding/tests/extensions.rs.
const OVERLAY_EXTENSION_MANIFEST: &str = r#"{"schemaVersion":1,"id":"overlay-smoke","runtime":"quickjs","entry":"index.mjs","capabilities":["overlays","ui","event_hooks"],"uiCapabilities":["overlay"]}"#;

const OVERLAY_EXTENSION_ENTRY: &str = r#"
export default function (pi) {
  pi.registerOverlay({
    id: "chat",
    title: "Side Chat",
    render: (ctx) => [
      "hello from overlay",
      { text: "styled row", style: "accent" },
      { text: "error row", style: "error" },
    ],
  });
}
"#;

struct OverlayProbe {
    child: std::process::Child,
    output: Arc<Mutex<String>>,
    writer: std::fs::File,
    // The isolated home and cwd must outlive the spawned child (rpi resolves
    // the current directory at startup).
    _home: TempDir,
    _cwd: TempDir,
}

impl OverlayProbe {
    fn spawn(extension_dir: &std::path::Path) -> Self {
        let home = TempDir::new().expect("temp home");
        let cwd = TempDir::new().expect("temp cwd");
        let pty = openpty(
            Some(&Winsize {
                ws_row: 24,
                ws_col: 100,
                ws_xpixel: 0,
                ws_ypixel: 0,
            }),
            None::<&Termios>,
        )
        .expect("openpty");
        let slave_in = pty.slave.try_clone().expect("slave in");
        let slave_out = pty.slave.try_clone().expect("slave out");
        let mut child = Command::new(rpi_bin())
            .env_clear()
            .env("HOME", home.path())
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("TERM", "xterm-256color")
            .env("PI_OFFLINE", "1")
            .env("PI_SKIP_VERSION_CHECK", "1")
            .args([
                "--model",
                "faux/faux-1",
                "--extension",
                extension_dir.to_str().expect("extension path"),
            ])
            .current_dir(cwd.path())
            .stdin(Stdio::from(slave_in))
            .stdout(Stdio::from(slave_out))
            .stderr(Stdio::from(pty.slave))
            .spawn()
            .expect("spawn rpi");
        let mut writer = std::fs::File::from(pty.master.try_clone().expect("master clone"));
        // Master must remain read/write so the capture thread can answer CSI 6n.
        let mut master = std::fs::File::from(pty.master);
        let output = Arc::new(Mutex::new(String::new()));
        let captured = output.clone();
        thread::spawn(move || {
            let mut bytes = [0; 8192];
            loop {
                match master.read(&mut bytes) {
                    Ok(0) => break,
                    Ok(count) => {
                        let chunk = &bytes[..count];
                        captured
                            .lock()
                            .expect("captured lock")
                            .push_str(&String::from_utf8_lossy(chunk));
                        // Ratatui inline viewport probes cursor position (CSI 6n).
                        // Bare PTYs have no emulator — answer row 1, column 1.
                        if chunk.windows(4).any(|window| window == b"\x1b[6n") {
                            let _ = master.write_all(b"\x1b[1;1R");
                            let _ = master.flush();
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        OverlayProbe {
            child,
            output,
            writer,
            _home: home,
            _cwd: cwd,
        }
    }

    fn wait_for(&self, needle: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.output.lock().expect("output lock").contains(needle) {
                return true;
            }
            thread::sleep(Duration::from_millis(25));
        }
        false
    }

    fn send(&mut self, input: &[u8]) {
        self.writer.write_all(input).expect("pty write");
        self.writer.flush().expect("pty flush");
    }
}

impl Drop for OverlayProbe {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start_overlay_probe() -> (OverlayProbe, TempDir) {
    let extension = TempDir::new().expect("extension dir");
    std::fs::write(
        extension.path().join("pi-extension.json"),
        OVERLAY_EXTENSION_MANIFEST,
    )
    .expect("manifest");
    std::fs::write(
        extension.path().join("index.mjs"),
        OVERLAY_EXTENSION_ENTRY,
    )
    .expect("entry");
    let mut probe = OverlayProbe::spawn(extension.path());
    assert!(
        probe.wait_for("pi (rs)", Duration::from_secs(30))
            || probe.wait_for("π", Duration::from_secs(5))
            || probe.wait_for("ready", Duration::from_secs(5))
            || probe.wait_for("Enter submit", Duration::from_secs(5)),
        "TUI did not start: {}",
        probe.output.lock().expect("output lock")
    );
    (probe, extension)
}

#[test]
fn pty_extension_overlay_opens_and_renders_registered_rows() {
    let (mut probe, _extension) = start_overlay_probe();

    // `/overlay chat` resolves the registered descriptor, invokes the JS
    // render, and opens the panel with the title and every row.
    probe.send(b"/overlay chat\r");
    assert!(
        probe.wait_for("Side Chat", Duration::from_secs(20)),
        "overlay panel title missing: {}",
        probe.output.lock().expect("output lock")
    );
    assert!(
        probe.wait_for("hello from overlay", Duration::from_secs(10)),
        "plain overlay row missing: {}",
        probe.output.lock().expect("output lock")
    );
    assert!(
        probe.wait_for("styled row", Duration::from_secs(10)),
        "styled overlay row missing: {}",
        probe.output.lock().expect("output lock")
    );
    assert!(
        probe.wait_for("error row", Duration::from_secs(10)),
        "error-styled overlay row missing: {}",
        probe.output.lock().expect("output lock")
    );

    // The overlay stays open and another render invocation still resolves.
    probe.send(b"/overlay chat\r");
    assert!(
        probe.wait_for("hello from overlay", Duration::from_secs(10)),
        "second overlay render missing: {}",
        probe.output.lock().expect("output lock")
    );
}

#[test]
fn pty_extension_overlay_unknown_id_is_actionable() {
    let (mut probe, _extension) = start_overlay_probe();

    probe.send(b"/overlay missing\r");
    assert!(
        probe.wait_for("Unknown extension overlay", Duration::from_secs(20)),
        "unknown overlay id must surface an actionable status: {}",
        probe.output.lock().expect("output lock")
    );
    // The registered overlay id is part of the actionable error.
    assert!(
        probe.output.lock().expect("output lock").contains("chat"),
        "the error must list registered overlays"
    );
}

#[test]
fn pty_extension_overlay_without_arguments_shows_usage() {
    let (mut probe, _extension) = start_overlay_probe();

    probe.send(b"/overlay\r");
    assert!(
        probe.wait_for("Usage: /overlay <id>", Duration::from_secs(20)),
        "bare /overlay must show usage: {}",
        probe.output.lock().expect("output lock")
    );
}
