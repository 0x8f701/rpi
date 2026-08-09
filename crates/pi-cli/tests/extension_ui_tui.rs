#![cfg(unix)]

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use nix::pty::{Winsize, openpty};
use nix::sys::termios::{SetArg, Termios, cfmakeraw, tcgetattr, tcsetattr};
use tempfile::TempDir;

fn rpi_bin() -> String { env!("CARGO_BIN_EXE_rpi").to_owned() }

fn type_chars(writer: &mut std::fs::File, text: &str) {
    for byte in text.bytes() {
        writer.write_all(&[byte]).unwrap();
        writer.flush().unwrap();
        thread::sleep(Duration::from_millis(35));
    }
}

#[test]
fn pty_quickjs_extension_select_then_input() {
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    let extension = cwd.path().join("dialog-extension");
    std::fs::create_dir(&extension).unwrap();
    std::fs::write(extension.join("pi-extension.json"), r#"{"schemaVersion":1,"id":"dialog-smoke","runtime":"quickjs","entry":"index.mjs","capabilities":["commands","ui"],"uiCapabilities":["select","input","notify"]}"#).unwrap();
    std::fs::write(extension.join("index.mjs"), r#"
export default function (pi) {
  pi.registerCommand("dialog-smoke", { description: "Exercise TUI dialogs", handler: async (_args, ctx) => {
    const selected = await ctx.ui.select("Choose target", [
      { value: "alpha-value", label: "Alpha label", description: "first" },
      { value: "beta-value", label: "Beta label", description: "second" },
    ]);
    const input = await ctx.ui.input("Enter suffix", "suffix");
    ctx.ui.notify(`result:${selected}:${input}`, "info");
  }});
}
"#).unwrap();
    let pty = openpty(Some(&Winsize { ws_row: 24, ws_col: 100, ws_xpixel: 0, ws_ypixel: 0 }), None::<&Termios>).unwrap();
    let mut slave_termios = tcgetattr(&pty.slave).unwrap();
    cfmakeraw(&mut slave_termios);
    tcsetattr(&pty.slave, SetArg::TCSANOW, &slave_termios).unwrap();
    let slave_in = pty.slave.try_clone().unwrap();
    let slave_out = pty.slave.try_clone().unwrap();
    let mut child = Command::new(rpi_bin()).env_clear().env("HOME", home.path()).env("PATH", std::env::var("PATH").unwrap_or_default()).env("TERM", "xterm-256color").env("PI_OFFLINE", "1").env("PI_SKIP_VERSION_CHECK", "1").args(["--model", "faux/faux-1", "--extension", extension.to_str().unwrap()]).current_dir(cwd.path()).stdin(Stdio::from(slave_in)).stdout(Stdio::from(slave_out)).stderr(Stdio::from(pty.slave)).spawn().unwrap();
    let mut writer = std::fs::File::from(pty.master.try_clone().unwrap());
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
                    captured.lock().unwrap().push_str(&String::from_utf8_lossy(chunk));
                    if chunk.windows(4).any(|window| window == b"\x1b[6n") {
                        let _ = master.write_all(b"\x1b[1;1R");
                        let _ = master.flush();
                    }
                }
                Err(_) => break,
            }
        }
    });
    let wait_for = |needle: &str, timeout: Duration| { let deadline = Instant::now() + timeout; while Instant::now() < deadline { if output.lock().unwrap().contains(needle) { return true; } thread::sleep(Duration::from_millis(25)); } false };
    assert!(
        wait_for("rpi", Duration::from_secs(30))
            || wait_for("π", Duration::from_secs(5))
            || wait_for("Ready", Duration::from_secs(5)),
        "TUI did not start: {}",
        output.lock().unwrap()
    );
    writer.write_all(b"/run dialog-smoke\r").unwrap();
    writer.flush().unwrap();
    assert!(wait_for("Choose target", Duration::from_secs(15)), "select missing: {}", output.lock().unwrap());
    writer.write_all(b"\x1b[B\r").unwrap(); writer.flush().unwrap();
    assert!(wait_for("Enter suffix", Duration::from_secs(15)), "input missing: {}", output.lock().unwrap());
    type_chars(&mut writer, "typed");
    writer.write_all(b"\r").unwrap(); writer.flush().unwrap();
    assert!(wait_for("result:beta-value:typed", Duration::from_secs(15)), "result missing: {}", output.lock().unwrap());
    writer.write_all(&[0x04]).unwrap(); writer.flush().unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline { if child.try_wait().unwrap().is_some() { return; } thread::sleep(Duration::from_millis(25)); }
    let _ = child.kill(); panic!("TUI did not exit");
}
