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

#[test]
fn pty_quickjs_extension_run_and_chain_commands() {
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    let extension = cwd.path().join("run-extension");
    std::fs::create_dir(&extension).unwrap();
    // Trusted installed extension: explicit --extension path with
    // pi-extension.json manifest (capabilities/commands) and QuickJS entry.
    std::fs::write(
        extension.join("pi-extension.json"),
        r#"{"schemaVersion":1,"id":"run-smoke","runtime":"quickjs","entry":"index.mjs","capabilities":["commands","ui"],"uiCapabilities":["notify"]}"#,
    )
    .unwrap();
    std::fs::write(
        extension.join("index.mjs"),
        r#"
export default function (pi) {
  pi.registerCommand("alpha", {
    description: "first",
    handler: async (args) => `alpha:${args || "none"}`,
  });
  pi.registerCommand("beta", {
    description: "second",
    handler: async (args) => `beta:${args || "none"}`,
  });
}
"#,
    )
    .unwrap();

    let pty = openpty(
        Some(&Winsize {
            ws_row: 24,
            ws_col: 100,
            ws_xpixel: 0,
            ws_ypixel: 0,
        }),
        None::<&Termios>,
    )
    .unwrap();
    let slave_in = pty.slave.try_clone().unwrap();
    let slave_out = pty.slave.try_clone().unwrap();
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
            extension.to_str().unwrap(),
        ])
        .current_dir(cwd.path())
        .stdin(Stdio::from(slave_in))
        .stdout(Stdio::from(slave_out))
        .stderr(Stdio::from(pty.slave))
        .spawn()
        .unwrap();
    let mut writer = std::fs::File::from(pty.master.try_clone().unwrap());
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
                        .unwrap()
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
    let wait_for = |needle: &str, timeout: Duration| {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if output.lock().unwrap().contains(needle) {
                return true;
            }
            thread::sleep(Duration::from_millis(25));
        }
        false
    };

    assert!(
        wait_for("pi (rs)", Duration::from_secs(30))
            || wait_for("π", Duration::from_secs(5))
            || wait_for("ready", Duration::from_secs(5))
            || wait_for("Enter submit", Duration::from_secs(5)),
        "TUI did not start: {}",
        output.lock().unwrap()
    );

    writer.write_all(b"/run alpha hello\r").unwrap();
    writer.flush().unwrap();
    assert!(
        wait_for("alpha:hello", Duration::from_secs(20)),
        "/run missing over trusted QuickJS extension command: {}",
        output.lock().unwrap()
    );

    let before_chain = output.lock().unwrap().clone();
    writer.write_all(b"/chain alpha one | beta two\r").unwrap();
    writer.flush().unwrap();
    assert!(
        wait_for("alpha:one", Duration::from_secs(20)),
        "chain alpha missing: {}",
        output.lock().unwrap()
    );
    assert!(
        wait_for("beta:two", Duration::from_secs(10)),
        "chain beta missing: {}",
        output.lock().unwrap()
    );
    let after_chain = output.lock().unwrap().clone();
    let chain_delta = after_chain
        .strip_prefix(&before_chain)
        .unwrap_or(&after_chain);
    let alpha_at = chain_delta
        .find("alpha:one")
        .expect("alpha:one must appear in chain output");
    let beta_at = chain_delta
        .find("beta:two")
        .expect("beta:two must appear in chain output");
    assert!(
        alpha_at < beta_at,
        "/chain must execute steps in order; delta={chain_delta}"
    );

    let before_unknown = output.lock().unwrap().clone();
    writer.write_all(b"/run missing-command\r").unwrap();
    writer.flush().unwrap();
    assert!(
        wait_for(
            "unknown or untrusted extension command",
            Duration::from_secs(15)
        ),
        "missing command must fail closed with trust error: {}",
        output.lock().unwrap()
    );
    let after_unknown = output.lock().unwrap().clone();
    let delta = after_unknown
        .strip_prefix(&before_unknown)
        .unwrap_or(&after_unknown);
    // The trusted-command guard must surface the exact failure in the TUI.
    assert!(
        delta.contains("unknown or untrusted extension command"),
        "unknown /run must fail through the trusted-command guard: {delta}"
    );

    writer.write_all(&[0x04]).unwrap();
    writer.flush().unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if child.try_wait().unwrap().is_some() {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    let _ = child.kill();
    panic!("TUI did not exit");
}
