#![cfg(unix)]

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use nix::pty::{Winsize, openpty};
use nix::sys::termios::Termios;
use tempfile::TempDir;

fn pi_bin() -> String { env!("CARGO_BIN_EXE_pi").to_owned() }

#[test]
fn pty_bun_extension_select_then_input() {
    if Command::new("bun").arg("--version").output().is_err() { return; }
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    let extension = cwd.path().join("dialog-extension");
    std::fs::create_dir(&extension).unwrap();
    std::fs::write(extension.join("pi-extension.json"), r#"{"schemaVersion":1,"id":"dialog-smoke","runtime":"bun","entry":"index.ts","capabilities":["commands","ui"],"uiCapabilities":["select","input","notify"]}"#).unwrap();
    std::fs::write(extension.join("index.ts"), r#"
export default function (pi: any) {
  pi.registerCommand("dialog-smoke", { description: "Exercise TUI dialogs", handler: async (_args: string, ctx: any) => {
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
    let slave_in = pty.slave.try_clone().unwrap();
    let slave_out = pty.slave.try_clone().unwrap();
    let mut child = Command::new(pi_bin()).env_clear().env("HOME", home.path()).env("PATH", std::env::var("PATH").unwrap_or_default()).env("TERM", "xterm-256color").env("PI_OFFLINE", "1").env("PI_SKIP_VERSION_CHECK", "1").args(["--model", "faux/faux-1", "--extension", extension.to_str().unwrap()]).current_dir(cwd.path()).stdin(Stdio::from(slave_in)).stdout(Stdio::from(slave_out)).stderr(Stdio::from(pty.slave)).spawn().unwrap();
    let mut writer = std::fs::File::from(pty.master.try_clone().unwrap());
    let mut reader = std::fs::File::from(pty.master);
    let output = Arc::new(Mutex::new(String::new()));
    let captured = output.clone();
    thread::spawn(move || { let mut bytes = [0; 8192]; while let Ok(count) = reader.read(&mut bytes) { if count == 0 { break; } captured.lock().unwrap().push_str(&String::from_utf8_lossy(&bytes[..count])); } });
    let wait_for = |needle: &str, timeout: Duration| { let deadline = Instant::now() + timeout; while Instant::now() < deadline { if output.lock().unwrap().contains(needle) { return true; } thread::sleep(Duration::from_millis(25)); } false };
    assert!(wait_for("pi (rs)", Duration::from_secs(30)), "TUI did not start: {}", output.lock().unwrap());
    writer.write_all(b"/dialog-smoke\r").unwrap(); writer.flush().unwrap();
    assert!(wait_for("Choose target", Duration::from_secs(15)), "select missing: {}", output.lock().unwrap());
    writer.write_all(b"\x1b[B\r").unwrap(); writer.flush().unwrap();
    assert!(wait_for("Enter suffix", Duration::from_secs(15)), "input missing: {}", output.lock().unwrap());
    writer.write_all(b"typed\r").unwrap(); writer.flush().unwrap();
    assert!(wait_for("result:beta-value:typed", Duration::from_secs(15)), "result missing: {}", output.lock().unwrap());
    writer.write_all(&[0x04]).unwrap(); writer.flush().unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline { if child.try_wait().unwrap().is_some() { return; } thread::sleep(Duration::from_millis(25)); }
    let _ = child.kill(); panic!("TUI did not exit");
}
