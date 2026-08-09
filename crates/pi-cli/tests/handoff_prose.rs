//! End-to-end `/handoff` coverage through the real REPL binary (`--mode
//! text`, piped stdio, offline faux provider). `/handoff --prose` renders the
//! deterministic envelope plus the summarizer prose paragraph; bare `/handoff`
//! stays envelope-only — the distinctive `PI_FAUX_RESPONSE` text would appear
//! in the output if any provider call ran, so its absence is the observable
//! proof that the envelope path never invokes the summarizer. Unknown flags
//! are rejected with typed usage on stderr.
//!
//! No live credentials or network: `PI_OFFLINE=1` with the built-in
//! `faux/faux-1` model and an isolated temp HOME.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use tempfile::TempDir;

/// Seeds every offline faux provider reply; must be distinctive enough to
/// prove prose presence/absence in the handoff block.
const FAUX_PROSE: &str = "handoff-prose-faux-reply";

/// Hard upper bound for one REPL run (startup + slash dispatch + exit).
const REPL_TIMEOUT: Duration = Duration::from_secs(30);

fn rpi_bin() -> String {
    env!("CARGO_BIN_EXE_rpi").to_owned()
}

/// Bytes retained from each pipe for diagnostics. Readers still drain to EOF
/// so a chatty child cannot fill the OS pipe; only the prefix is kept.
fn drain_capped(mut read: impl Read, cap: usize) -> Vec<u8> {
    let mut retained = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match read.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                if retained.len() < cap {
                    let take = n.min(cap - retained.len());
                    retained.extend_from_slice(&chunk[..take]);
                }
            }
            Err(_) => break,
        }
    }
    retained
}

/// Run `rpi --mode text` with `stdin` piped in (then closed), a fresh temp
/// HOME, and the offline faux provider. Returns `(exit_ok, stdout, stderr)`.
/// Kills the child and panics with captured output if it does not exit within
/// [`REPL_TIMEOUT`].
fn run_repl(home: &Path, cwd: &Path, stdin: &str) -> (bool, String, String) {
    use std::io::Write as _;

    let mut child = Command::new(rpi_bin())
        .args(["--mode", "text", "--model", "faux/faux-1"])
        .arg("--cwd")
        .arg(cwd)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("PI_OFFLINE", "1")
        .env("PI_SKIP_VERSION_CHECK", "1")
        .env("PI_FAUX_RESPONSE", FAUX_PROSE)
        .env_remove("PI_PROFILE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn rpi REPL");
    {
        let mut sin = child.stdin.take().expect("stdin pipe");
        sin.write_all(stdin.as_bytes()).expect("write REPL stdin");
    } // drop closes stdin -> the REPL sees EOF after the commands.

    let stdout = child.stdout.take().expect("stdout pipe");
    let stderr = child.stderr.take().expect("stderr pipe");
    let stdout_reader = thread::spawn(move || drain_capped(stdout, 64 * 1024));
    let stderr_reader = thread::spawn(move || drain_capped(stderr, 64 * 1024));

    let deadline = Instant::now() + REPL_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                let stdout =
                    String::from_utf8_lossy(&stdout_reader.join().unwrap_or_default()).into_owned();
                let stderr =
                    String::from_utf8_lossy(&stderr_reader.join().unwrap_or_default()).into_owned();
                panic!(
                    "REPL did not exit within {:?}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
                    REPL_TIMEOUT
                );
            }
        }
    };
    let stdout = String::from_utf8_lossy(&stdout_reader.join().unwrap_or_default()).into_owned();
    let stderr = String::from_utf8_lossy(&stderr_reader.join().unwrap_or_default()).into_owned();
    (status.success(), stdout, stderr)
}

/// `/handoff --prose` through the real REPL: the envelope renders and the
/// summarizer prose (the faux reply) appears as the quoted paragraph — proof
/// the prose flag reaches `generate_handoff_with_prose` and prints.
#[test]
fn repl_handoff_prose_renders_envelope_plus_prose() {
    let home = TempDir::new().expect("temp HOME");
    let cwd = TempDir::new().expect("temp cwd");
    let (ok, stdout, stderr) = run_repl(home.path(), cwd.path(), "/handoff --prose\n/quit\n");
    assert!(
        ok,
        "REPL must exit 0\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );
    assert!(
        stdout.contains("# Handoff"),
        "envelope must render:\n{stdout}"
    );
    assert!(
        stdout.contains(&format!("> {FAUX_PROSE}")),
        "summarizer prose must render as the quoted paragraph:\n{stdout}"
    );
    assert!(
        stdout.contains("## Next steps"),
        "the block must include the prose section:\n{stdout}"
    );
}

/// Bare `/handoff` through the real REPL: envelope-only — the faux reply never
/// appears, which is only possible if no provider (summarizer) call ran.
#[test]
fn repl_handoff_bare_stays_envelope_only_without_provider_call() {
    let home = TempDir::new().expect("temp HOME");
    let cwd = TempDir::new().expect("temp cwd");
    let (ok, stdout, stderr) = run_repl(home.path(), cwd.path(), "/handoff\n/quit\n");
    assert!(
        ok,
        "REPL must exit 0\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );
    assert!(
        stdout.contains("# Handoff"),
        "envelope must render:\n{stdout}"
    );
    assert!(
        !stdout.contains(FAUX_PROSE),
        "bare /handoff must not invoke the provider (no prose may appear):\n{stdout}"
    );
}

/// An unknown `/handoff` flag is rejected with typed usage on stderr and
/// renders no handoff block.
#[test]
fn repl_handoff_unknown_flag_is_rejected_with_usage() {
    let home = TempDir::new().expect("temp HOME");
    let cwd = TempDir::new().expect("temp cwd");
    let (ok, stdout, stderr) = run_repl(home.path(), cwd.path(), "/handoff --bogus\n/quit\n");
    assert!(
        ok,
        "REPL must exit 0\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );
    assert!(
        !stdout.contains("# Handoff"),
        "a rejected flag must not render a handoff block:\n{stdout}"
    );
    assert!(
        stderr.contains("/handoff [--prose]"),
        "usage must reach stderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("panicked"),
        "rejection must be a typed error, not a panic:\n{stderr}"
    );
}
