//! Office/notebook text extraction for the read tool.
//!
//! Mirrors the PDF branch's external-converter pattern: a short-lived child
//! process (pandoc / LibreOffice / jupyter nbconvert) extracts the text layer,
//! under the same deadline, hard output cap, abort handling, and actionable
//! missing-converter rejections. Formats are detected by extension — unlike
//! images/PDF there is no cheap magic-byte signature that reliably
//! distinguishes these container formats.
//!
//! Converter matrix (availability is checked at runtime, not build time):
//! - `.docx/.xlsx/.pptx/.odt/.ods/.odp/.rtf` → `pandoc -t plain` (preferred;
//!   covers every spreadsheet sheet) or `libreoffice --headless --convert-to
//!   txt` (fallback; exports only the first spreadsheet sheet).
//! - `.epub` → `pandoc -t plain` only (LibreOffice cannot open EPUB).
//! - `.ipynb` → `jupyter nbconvert --to script --stdout`.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, Result};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use pi_agent::AbortSignal;

use super::{copy_capped, spawn_with_etxtbsy_retry};
use crate::truncate::format_size;

/// Deadline for pandoc/nbconvert conversions (mirrors `PDF_EXTRACT_TIMEOUT`).
pub(crate) const DOC_EXTRACT_TIMEOUT: Duration = Duration::from_secs(30);
/// LibreOffice cold-starts (soffice bootstrap, fresh user profile) run slower
/// than the other converters; give them twice the default deadline.
const LIBREOFFICE_EXTRACT_TIMEOUT: Duration = Duration::from_secs(60);
/// Hard cap on bytes read from a converter's stdout (or LibreOffice's output
/// file). Mirrors `PDF_EXTRACT_MAX_BYTES`; a runaway or malicious document
/// cannot exhaust the heap.
pub(crate) const DOC_EXTRACT_MAX_BYTES: usize = 32 * 1024 * 1024;

/// Formats the read tool converts to text, keyed by extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DocKind {
    /// `.ipynb` Jupyter notebooks (`jupyter nbconvert --to script --stdout`).
    Notebook,
    /// `.epub` ebooks (pandoc only).
    Epub,
    /// Word/Calc/Impress/ODF/RTF documents (pandoc, LibreOffice fallback).
    Office,
}

/// Maps a file extension (case-insensitive) to a conversion kind. Extension
/// detection is enough for these formats: they have no single cheap magic
/// signature, and a mismatched extension simply fails the converter's own
/// parse.
pub(crate) fn doc_kind(path: &Path) -> Option<DocKind> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "ipynb" => Some(DocKind::Notebook),
        "epub" => Some(DocKind::Epub),
        "docx" | "xlsx" | "pptx" | "odt" | "ods" | "odp" | "rtf" => Some(DocKind::Office),
        _ => None,
    }
}

/// Whether the read tool routes `path` through the document converters.
pub(crate) fn is_doc(path: &Path) -> bool {
    doc_kind(path).is_some()
}

/// The converter that produced the text, so the read tool's `sed -n` escape
/// hatch can re-run the same command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DocConverter {
    Pandoc,
    LibreOffice,
    Nbconvert,
}

impl DocConverter {
    /// Shell process substitution that re-extracts the same text, spliced into
    /// `sed -n 'Np' <hint> | head -c N` (the read tool's single-oversized-line
    /// escape hatch). `quoted_abs` is the single-quoted absolute path.
    pub(crate) fn sed_hint(&self, quoted_abs: &str) -> String {
        match self {
            DocConverter::Pandoc => format!("<(pandoc -t plain {quoted_abs})"),
            DocConverter::Nbconvert => {
                format!("<(jupyter nbconvert --to script --stdout {quoted_abs})")
            }
            // LibreOffice writes a file rather than stdout; the substitution
            // re-runs the conversion into a fresh scratch dir and cats the
            // result. The `;` makes it a subshell script inside the <(...).
            DocConverter::LibreOffice => format!(
                "<(d=$(mktemp -d); libreoffice --headless -env:UserInstallation=\"file://$d/profile\" --convert-to \"txt:Text (encoded):UTF8\" --outdir \"$d\" {quoted_abs} >/dev/null 2>&1; cat \"$d\"/*.txt)"
            ),
        }
    }
}

/// Extracted text plus the converter that produced it.
#[derive(Debug)]
pub(crate) struct ExtractedDoc {
    pub(crate) text: String,
    pub(crate) converter: DocConverter,
}

/// Extracts text from an Office document or notebook, dispatching on the
/// extension. Errors are actionable: a missing converter names the command and
/// an install hint; a nonzero exit, timeout, or cancellation each carry a bash
/// alternative (mirrors `extract_pdf_text`).
pub(crate) async fn extract_doc_text(abs: &str, abort: AbortSignal) -> Result<ExtractedDoc> {
    match doc_kind(Path::new(abs)) {
        Some(DocKind::Notebook) => extract_notebook_text_with("jupyter", abs, abort).await,
        Some(DocKind::Epub) => extract_pandoc_text_with("pandoc", abs, abort).await,
        Some(DocKind::Office) => extract_office_text_with("pandoc", "libreoffice", abs, abort).await,
        None => Err(anyhow!("unsupported document type: {abs}")),
    }
}

/// `jupyter nbconvert --to script --stdout <abs>`: code cells in execution
/// order, markdown cells as comments. Injectable binary name for tests.
pub(crate) async fn extract_notebook_text_with(
    bin: &str,
    abs: &str,
    abort: AbortSignal,
) -> Result<ExtractedDoc> {
    let args = vec![
        "nbconvert".to_string(),
        "--to".to_string(),
        "script".to_string(),
        "--stdout".to_string(),
        abs.to_string(),
    ];
    let use_bash = format!("jupyter nbconvert --to script --stdout '{}'", abs);
    let run = match run_converter(bin, &args, DOC_EXTRACT_TIMEOUT, abort, &use_bash).await {
        Ok(Some(run)) => run,
        Ok(None) => {
            return Err(anyhow!(
                "Reading Jupyter notebooks requires jupyter nbconvert; install it (e.g. `pip install nbconvert`) or use bash: {}",
                use_bash
            ));
        }
        Err(e) => return Err(e),
    };
    finish_stdout_run(bin, abs, run, DocConverter::Nbconvert, &use_bash)
}

/// `pandoc -t plain <abs>` — plain-text output keeps the extracted text
/// layout-stable for the read tool's offset/limit selection. Injectable binary
/// name for tests; used directly for EPUB (pandoc-only) and as the preferred
/// path for Office documents.
pub(crate) async fn extract_pandoc_text_with(
    bin: &str,
    abs: &str,
    abort: AbortSignal,
) -> Result<ExtractedDoc> {
    let args = vec!["-t".to_string(), "plain".to_string(), abs.to_string()];
    let use_bash = format!("pandoc -t plain '{}'", abs);
    let run = match run_converter(bin, &args, DOC_EXTRACT_TIMEOUT, abort, &use_bash).await {
        Ok(Some(run)) => run,
        Ok(None) => {
            return Err(anyhow!(
                "Reading EPUB files requires pandoc; install it (e.g. `apt install pandoc`) or use bash: {}",
                use_bash
            ));
        }
        Err(e) => return Err(e),
    };
    finish_stdout_run(bin, abs, run, DocConverter::Pandoc, &use_bash)
}

/// Office text via `pandoc` (preferred) with a `libreoffice --headless
/// --convert-to txt` fallback when pandoc is not installed. A pandoc
/// *failure* (nonzero exit, timeout, abort) is reported, not masked by the
/// fallback — only a missing binary falls through. Injectable binary names for
/// tests.
pub(crate) async fn extract_office_text_with(
    pandoc_bin: &str,
    libreoffice_bin: &str,
    abs: &str,
    abort: AbortSignal,
) -> Result<ExtractedDoc> {
    let pandoc_args = vec!["-t".to_string(), "plain".to_string(), abs.to_string()];
    let pandoc_bash = format!("pandoc -t plain '{}'", abs);
    match run_converter(pandoc_bin, &pandoc_args, DOC_EXTRACT_TIMEOUT, abort.clone(), &pandoc_bash)
        .await
    {
        Ok(Some(run)) => {
            return finish_stdout_run(pandoc_bin, abs, run, DocConverter::Pandoc, &pandoc_bash);
        }
        Ok(None) => {} // pandoc not installed → LibreOffice fallback below.
        Err(e) => return Err(e),
    }
    if let Some(doc) = libreoffice_attempt(libreoffice_bin, abs, abort).await? {
        return Ok(doc);
    }
    Err(anyhow!(
        "Reading Office documents requires pandoc or LibreOffice; install one (e.g. `apt install pandoc` or `apt install libreoffice-writer libreoffice-calc libreoffice-impress`) or use bash: {}",
        pandoc_bash
    ))
}

/// `libreoffice --headless -env:UserInstallation=<fresh profile> --convert-to
/// "txt:Text (encoded):UTF8" --outdir <scratch> <abs>`, then reads the
/// produced `.txt` back. Returns `Ok(None)` when the binary is not installed.
/// A fresh scratch dir per conversion isolates the user profile (parallel
/// conversions do not contend for a profile lock) and keeps the output away
/// from the source directory.
async fn libreoffice_attempt(
    bin: &str,
    abs: &str,
    abort: AbortSignal,
) -> Result<Option<ExtractedDoc>> {
    let tmp = std::env::temp_dir().join(format!("pi-doc-convert-{}", uuid::Uuid::new_v4()));
    let outdir = tmp.join("out");
    // Drop guard: the scratch dir is removed on every exit path — including a
    // panic mid-conversion, which a closure-based cleanup would leak.
    let mut scratch = ScratchDir(tmp.clone());
    std::fs::create_dir_all(&outdir)
        .map_err(|e| anyhow!("failed to create LibreOffice scratch dir: {e}"))?;
    let args = vec![
        "--headless".to_string(),
        format!("-env:UserInstallation={}", file_url(&tmp.join("profile"))),
        "--convert-to".to_string(),
        "txt:Text (encoded):UTF8".to_string(),
        "--outdir".to_string(),
        outdir.to_string_lossy().into_owned(),
        abs.to_string(),
    ];
    let use_bash = format!(
        "libreoffice --headless --convert-to txt --outdir <dir> '{}'",
        abs
    );
    let run = match run_converter(bin, &args, LIBREOFFICE_EXTRACT_TIMEOUT, abort, &use_bash).await
    {
        Ok(Some(run)) => run,
        Ok(None) => {
            scratch.cleanup();
            return Ok(None);
        }
        Err(e) => {
            scratch.cleanup();
            return Err(e);
        }
    };
    let (out, end) = run;
    match end {
        ConverterEnd::Exited(status) if !status.success() => {
            let status_desc = match status.code() {
                Some(code) => format!("exit status {code}"),
                None => "terminated by signal".to_string(),
            };
            let detail = String::from_utf8_lossy(&out.stderr).trim().to_string();
            scratch.cleanup();
            return Err(anyhow!(
                "libreoffice failed ({status_desc}): {detail}. Use bash: {} to inspect (the file may be corrupted or password-protected).",
                use_bash
            ));
        }
        ConverterEnd::Exited(_) | ConverterEnd::Capped => {}
    }
    // --convert-to writes a file; stdout is empty. Read the produced .txt
    // (capped) back from the scratch dir.
    let (bytes, capped) = match read_single_txt(&outdir).await {
        Ok(Some(found)) => found,
        Ok(None) => {
            scratch.cleanup();
            return Err(anyhow!("libreoffice produced no output for {abs}"));
        }
        Err(e) => {
            scratch.cleanup();
            return Err(anyhow!("failed to read LibreOffice output: {e}"));
        }
    };
    scratch.cleanup();
    // The UTF-8 text filter emits a leading BOM; it is a filter artifact, not
    // document content.
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if let Some(stripped) = text.strip_prefix('\u{feff}') {
        text = stripped.to_owned();
    }
    if text.trim().is_empty() {
        return Err(anyhow!(
            "{abs} contains no extractable text (empty, image-only, or password-protected)"
        ));
    }
    Ok(Some(ExtractedDoc {
        text: apply_doc_cap_notice(text, capped, bin),
        converter: DocConverter::LibreOffice,
    }))
}

/// Owns a LibreOffice scratch directory for the duration of a conversion.
/// [`Drop`] removes the directory on every exit path — including a panic
/// mid-conversion, which a closure-based cleanup would leak. `cleanup` also
/// removes it eagerly (idempotent; Drop then no-ops).
struct ScratchDir(PathBuf);

impl ScratchDir {
    fn cleanup(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        self.cleanup();
    }
}

/// Capped stdout/stderr of a completed converter run.
struct ConverterOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    stdout_capped: bool,
}

/// How a converter run ended.
enum ConverterEnd {
    /// The process exited on its own; the status is authoritative.
    Exited(std::process::ExitStatus),
    /// The stdout cap was hit and the process group was killed; stdout holds
    /// the first `DOC_EXTRACT_MAX_BYTES` bytes.
    Capped,
}

/// SIGKILLs the whole process group led by `pid`. The converter is spawned as
/// a group leader (`process_group(0)`), so its descendants — LibreOffice
/// spawns helper processes that inherit the group — are reaped by the same
/// kill; a leader-only `kill()` would leave them running with the output pipes
/// open. Best-effort: the group may already be gone (ESRCH). Mirrors
/// `hooks.rs::kill_process_group`.
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

/// Spawns `bin` with `args`, collects capped stdout and raw stderr, and races
/// the wait against `timeout`/`abort` (same shape as `extract_pdf_text_with`).
/// Returns `Ok(None)` when the binary is not on PATH, `Ok(Some(..))` on a
/// completed or capped run, and an actionable `Err` for timeouts, aborts, and
/// wait failures. `use_bash` is spliced into the timeout message as the
/// manual-command alternative.
async fn run_converter(
    bin: &str,
    args: &[String],
    timeout: Duration,
    abort: AbortSignal,
    use_bash: &str,
) -> Result<Option<(ConverterOutput, ConverterEnd)>> {
    let mut cmd = Command::new(bin);
    cmd.args(args);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.kill_on_drop(true);
    // Own process group: a timeout/abort/cap kill reaps the converter and any
    // descendants sharing the output pipes.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.as_std_mut().process_group(0);
    }
    let mut child = match spawn_with_etxtbsy_retry(&mut cmd, bin).await {
        Ok(child) => child,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(anyhow!("failed to start {bin}: {e}")),
    };
    // Capture the leader pid before the race so the timeout/abort/cap kill
    // below can SIGKILL the whole process group; the converter may have
    // spawned descendants (LibreOffice helpers) that share the group.
    let child_pid = child.id();

    // Collect stdout/stderr on separate tasks so `child` stays available for
    // the timeout/abort kill. stdout is hard-capped at
    // `DOC_EXTRACT_MAX_BYTES`; the (out, capped) pair lets the caller attach
    // the cap notice.
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let mut out_task = tokio::spawn(async move {
        let mut out = Vec::new();
        let capped = copy_capped(&mut stdout, &mut out, DOC_EXTRACT_MAX_BYTES)
            .await
            .unwrap_or(false);
        (out, capped)
    });
    let err_task = tokio::spawn(async move {
        let mut err = Vec::new();
        let _ = tokio::io::copy(&mut stderr, &mut err).await;
        err
    });

    // Timeout, abort, and the stdout cap win over conversion; each kills the
    // process group so the piped output closes and the readers finish. The
    // wait future is scoped so its `&mut child` borrow ends before the kill
    // below. `biased` with the abort branch first makes an already-cancelled
    // signal deterministically win over a fast child exit.
    enum RunOutcome {
        Exited(std::io::Result<std::process::ExitStatus>),
        TimedOut,
        Aborted,
        // The stdout reader finished inside the select; its JoinHandle output
        // is carried here so the handle is never polled twice (polling a
        // completed JoinHandle panics).
        Capped(Result<(Vec<u8>, bool), tokio::task::JoinError>),
    }
    let outcome = {
        let wait = child.wait();
        tokio::pin!(wait);
        let sleep = tokio::time::sleep(timeout);
        tokio::pin!(sleep);
        let abort_fut = abort.cancelled();
        tokio::pin!(abort_fut);
        tokio::select! {
            biased;
            _ = &mut abort_fut => RunOutcome::Aborted,
            _ = &mut sleep => RunOutcome::TimedOut,
            res = &mut wait => RunOutcome::Exited(res),
            joined = &mut out_task => RunOutcome::Capped(joined),
        }
    };

    // On timeout/abort/cap SIGKILL the whole process group (the leader pid
    // captured before the select); the readers finish when the killed
    // processes close the pipes. The cap kill is required: we stopped
    // draining stdout, so leaving the child running would block it on a full
    // pipe until the timeout. `wait` reaps the leader after the group kill.
    if matches!(
        &outcome,
        RunOutcome::TimedOut | RunOutcome::Aborted | RunOutcome::Capped(_)
    ) {
        kill_process_group(child_pid);
        let _ = child.wait().await;
    }
    let stderr = err_task.await.map_err(|e| anyhow!("{bin} error task failed: {e}"))?;
    let (stdout, capped, end) = match outcome {
        // The Exited branch won the biased select before out_task was polled,
        // so awaiting the handle here is safe (it has not completed).
        RunOutcome::Exited(Ok(status)) => {
            let (stdout, capped) = out_task
                .await
                .map_err(|e| anyhow!("{bin} output task failed: {e}"))?;
            (stdout, capped, ConverterEnd::Exited(status))
        }
        RunOutcome::Exited(Err(e)) => return Err(anyhow!("failed to wait for {bin}: {e}")),
        // The Capped branch already consumed the handle output in the select;
        // reuse it instead of polling the handle again. Its status reflects
        // the cap kill, not a conversion failure, so nothing to check.
        RunOutcome::Capped(joined) => {
            let (stdout, capped) =
                joined.map_err(|e| anyhow!("{bin} output task failed: {e}"))?;
            (stdout, capped, ConverterEnd::Capped)
        }
        RunOutcome::TimedOut => {
            return Err(anyhow!(
                "{bin} timed out after {}s. Use bash: {use_bash} (the file may be corrupted or password-protected)",
                timeout.as_secs()
            ));
        }
        RunOutcome::Aborted => return Err(anyhow!("{bin} conversion cancelled")),
    };
    Ok(Some((
        ConverterOutput {
            stdout,
            stderr,
            stdout_capped: capped,
        },
        end,
    )))
}

/// Turns a completed stdout-producing run into extracted text, or an
/// actionable failure for a nonzero exit or empty output.
fn finish_stdout_run(
    bin: &str,
    abs: &str,
    run: (ConverterOutput, ConverterEnd),
    converter: DocConverter,
    use_bash: &str,
) -> Result<ExtractedDoc> {
    let (out, end) = run;
    match end {
        ConverterEnd::Exited(status) if !status.success() => {
            let status_desc = match status.code() {
                Some(code) => format!("exit status {code}"),
                None => "terminated by signal".to_string(),
            };
            let detail = String::from_utf8_lossy(&out.stderr).trim().to_string();
            Err(anyhow!(
                "{bin} failed ({status_desc}): {detail}. Use bash: {use_bash} to inspect (the file may be corrupted or password-protected)."
            ))
        }
        ConverterEnd::Exited(_) | ConverterEnd::Capped => {
            let text = String::from_utf8_lossy(&out.stdout).into_owned();
            if text.trim().is_empty() {
                Err(anyhow!(
                    "{abs} contains no extractable text (empty, image-only, or password-protected)"
                ))
            } else {
                Ok(ExtractedDoc {
                    text: apply_doc_cap_notice(text, out.stdout_capped, bin),
                    converter,
                })
            }
        }
    }
}

/// Prepends the cap notice to extracted text when the extraction bound was
/// hit. Prefixed (not appended) because `render_read_result` truncates from
/// the head, so the notice must lead to stay visible.
fn apply_doc_cap_notice(mut text: String, capped: bool, bin: &str) -> String {
    if capped {
        text.insert_str(
            0,
            &format!("[{bin} output capped at {}]\n", format_size(DOC_EXTRACT_MAX_BYTES)),
        );
    }
    text
}

/// Reads the single `.txt` file LibreOffice wrote into `dir`, capped at
/// `DOC_EXTRACT_MAX_BYTES`. `None` when no `.txt` appeared.
async fn read_single_txt(dir: &Path) -> std::io::Result<Option<(Vec<u8>, bool)>> {
    let mut found: Option<PathBuf> = None;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.path().extension().is_some_and(|ext| ext == "txt") {
            found = Some(entry.path());
            break;
        }
    }
    let Some(path) = found else { return Ok(None) };
    let mut file = tokio::fs::File::open(&path).await?;
    let mut out = Vec::new();
    let capped = copy_capped(&mut file, &mut out, DOC_EXTRACT_MAX_BYTES).await?;
    Ok(Some((out, capped)))
}

/// Builds a `file://` URL from a path (percent-encodes anything outside the
/// URL-safe set) for LibreOffice's `-env:UserInstallation` option.
fn file_url(path: &Path) -> String {
    let mut out = String::from("file://");
    for byte in path.to_string_lossy().bytes() {
        match byte {
            b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z' | b'/' | b'-' | b'_' | b'.' | b'~' => {
                out.push(char::from(byte))
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_path(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
    }

    fn abort_ctx() -> AbortSignal {
        let (_ctrl, abort) = pi_agent::AbortController::new();
        std::mem::forget(_ctrl);
        abort
    }

    fn pandoc_available() -> bool {
        std::process::Command::new("pandoc")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    fn libreoffice_available() -> bool {
        std::process::Command::new("libreoffice")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    #[test]
    fn doc_kind_extension_table() {
        for (name, expected) in [
            ("notes.ipynb", Some(DocKind::Notebook)),
            ("book.epub", Some(DocKind::Epub)),
            ("a.docx", Some(DocKind::Office)),
            ("b.xlsx", Some(DocKind::Office)),
            ("c.pptx", Some(DocKind::Office)),
            ("d.odt", Some(DocKind::Office)),
            ("e.ods", Some(DocKind::Office)),
            ("f.odp", Some(DocKind::Office)),
            ("g.rtf", Some(DocKind::Office)),
            // Case-insensitive.
            ("UPPER.DOCX", Some(DocKind::Office)),
            ("NOTE.IPYNB", Some(DocKind::Notebook)),
            // Everything else stays on the text path.
            ("readme.md", None),
            ("code.rs", None),
            ("data.json", None),
            ("photo.png", None),
            ("archive.tar.gz", None),
            ("noextension", None),
        ] {
            assert_eq!(doc_kind(Path::new(name)), expected, "extension table for {name}");
        }
    }

    #[tokio::test]
    async fn extract_notebook_missing_binary_is_actionable() {
        let err = extract_notebook_text_with(
            "jupyter-definitely-missing-xyz",
            "/nonexistent/nb.ipynb",
            abort_ctx(),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("jupyter nbconvert"), "got: {err}");
        assert!(err.contains("pip install nbconvert"), "got: {err}");
    }

    #[tokio::test]
    async fn extract_epub_missing_binary_is_actionable() {
        let err = extract_pandoc_text_with(
            "pandoc-definitely-missing-xyz",
            "/nonexistent/book.epub",
            abort_ctx(),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("Reading EPUB files requires pandoc"), "got: {err}");
        assert!(err.contains("apt install pandoc"), "got: {err}");
    }

    #[tokio::test]
    async fn extract_office_missing_both_binaries_is_actionable() {
        let err = extract_office_text_with(
            "pandoc-definitely-missing-xyz",
            "libreoffice-definitely-missing-xyz",
            "/nonexistent/doc.docx",
            abort_ctx(),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("pandoc"), "got: {err}");
        assert!(err.contains("LibreOffice"), "got: {err}");
        assert!(err.contains("apt install"), "got: {err}");
    }

    #[tokio::test]
    async fn extract_office_real_failure_does_not_fall_back() {
        // `false` exists on PATH and exits nonzero immediately; the office
        // path must report that failure instead of silently falling back to a
        // (deliberately missing) libreoffice binary.
        let err = extract_office_text_with(
            "false",
            "libreoffice-definitely-missing-xyz",
            "/nonexistent/doc.docx",
            abort_ctx(),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("false failed (exit status 1)"), "got: {err}");
        assert!(!err.contains("LibreOffice"), "got: {err}");
    }

    #[tokio::test]
    async fn extract_pandoc_docx_text() {
        if !pandoc_available() {
            eprintln!("skipping: pandoc not available");
            return;
        }
        let fixture = fixture_path("sample.docx");
        assert!(fixture.is_file(), "missing fixture {}", fixture.display());
        let doc = extract_pandoc_text_with("pandoc", fixture.to_str().unwrap(), abort_ctx())
            .await
            .unwrap();
        assert_eq!(doc.converter, DocConverter::Pandoc);
        for i in 1..=6 {
            assert!(
                doc.text.contains(&format!("Line {i} of docx fixture")),
                "missing line {i} in: {}",
                doc.text
            );
        }
    }

    #[tokio::test]
    async fn extract_office_prefers_pandoc_over_libreoffice() {
        if !pandoc_available() {
            eprintln!("skipping: pandoc not available");
            return;
        }
        let fixture = fixture_path("sample.docx");
        // pandoc present, libreoffice deliberately missing → pandoc wins.
        let doc = extract_office_text_with(
            "pandoc",
            "libreoffice-definitely-missing-xyz",
            fixture.to_str().unwrap(),
            abort_ctx(),
        )
        .await
        .unwrap();
        assert_eq!(doc.converter, DocConverter::Pandoc);
        assert!(doc.text.contains("Line 1 of docx fixture"), "{}", doc.text);
    }

    #[tokio::test]
    async fn extract_office_falls_back_to_libreoffice_when_pandoc_missing() {
        if !libreoffice_available() {
            eprintln!("skipping: libreoffice not available");
            return;
        }
        let fixture = fixture_path("sample.docx");
        // pandoc deliberately missing → the LibreOffice fallback must fire.
        let doc = extract_office_text_with(
            "pandoc-definitely-missing-xyz",
            "libreoffice",
            fixture.to_str().unwrap(),
            abort_ctx(),
        )
        .await
        .unwrap();
        assert_eq!(doc.converter, DocConverter::LibreOffice);
        assert!(
            !doc.text.starts_with('\u{feff}'),
            "BOM must be stripped: {:?}",
            doc.text
        );
        for i in 1..=6 {
            assert!(
                doc.text.contains(&format!("Line {i} of docx fixture")),
                "missing line {i} in: {}",
                doc.text
            );
        }
    }

    #[tokio::test]
    async fn extract_doc_abort_cancels_conversion() {
        if !pandoc_available() && !libreoffice_available() {
            eprintln!("skipping: no converter available");
            return;
        }
        let fixture = fixture_path("sample.docx");
        let (ctrl, abort) = pi_agent::AbortController::new();
        ctrl.abort();
        let err = extract_doc_text(fixture.to_str().unwrap(), abort)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("cancelled"), "got: {err}");
    }

    #[test]
    fn doc_cap_notice_prefixes_and_names_converter() {
        assert_eq!(apply_doc_cap_notice("body".to_owned(), false, "pandoc"), "body");
        let notice = apply_doc_cap_notice(String::new(), true, "pandoc");
        assert!(
            notice.starts_with("[pandoc output capped at 32.0MB]"),
            "cap notice must name the bound and converter: {notice:?}"
        );
    }

    #[test]
    fn file_url_encodes_special_characters() {
        assert_eq!(file_url(Path::new("/tmp/plain")), "file:///tmp/plain");
        assert_eq!(file_url(Path::new("/tmp/sp ace")), "file:///tmp/sp%20ace");
    }

    #[test]
    fn sed_hint_names_the_same_converter() {
        let q = "'/tmp/a file.docx'";
        assert_eq!(
            DocConverter::Pandoc.sed_hint(q),
            "<(pandoc -t plain '/tmp/a file.docx')"
        );
        assert_eq!(
            DocConverter::Nbconvert.sed_hint(q),
            "<(jupyter nbconvert --to script --stdout '/tmp/a file.docx')"
        );
        assert!(
            DocConverter::LibreOffice.sed_hint(q).starts_with("<(d=$(mktemp -d); libreoffice --headless"),
            "LibreOffice hint must re-run the conversion into a scratch dir"
        );
        assert!(
            DocConverter::LibreOffice.sed_hint(q).ends_with("cat \"$d\"/*.txt)"),
            "LibreOffice hint must cat the produced txt"
        );
    }

    fn write_fake_converter(dir: &Path, name: &str, script: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, script).expect("write fake converter");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&path).expect("metadata").permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&path, permissions).expect("chmod fake converter");
        }
        path
    }

    #[test]
    fn scratch_dir_cleanup_removes_dir_and_drop_is_idempotent() {
        let dir = tempfile::tempdir().expect("temp dir");
        let scratch = dir.path().join("scratch");
        std::fs::create_dir_all(&scratch).expect("create scratch");
        let mut guard = ScratchDir(scratch.clone());
        guard.cleanup();
        assert!(!scratch.exists(), "eager cleanup must remove the dir");
        drop(guard);
        assert!(!scratch.exists(), "drop of a cleaned guard must stay idempotent");
    }

    #[test]
    fn scratch_dir_drop_removes_dir_on_panic() {
        let dir = tempfile::tempdir().expect("temp dir");
        let scratch = dir.path().join("scratch");
        std::fs::create_dir_all(&scratch).expect("create scratch");
        let outcome = std::panic::catch_unwind(|| {
            let _guard = ScratchDir(scratch.clone());
            panic!("conversion panicked mid-run");
        });
        assert!(outcome.is_err(), "the panic must propagate");
        assert!(
            !scratch.exists(),
            "scratch dir must be removed even when the conversion panics"
        );
    }

    #[tokio::test]
    async fn libreoffice_attempt_success_cleans_scratch_dir() {
        let dir = tempfile::tempdir().expect("temp dir");
        let abs = dir.path().join("sample.docx");
        std::fs::write(&abs, "ignored by the fake converter").expect("write source");
        // Fake libreoffice: writes the .txt into the --outdir and records that
        // outdir next to the source document so the test can verify the
        // scratch root is gone after the conversion.
        let script = r#"#!/bin/sh
out=""
prev=""
for arg in "$@"; do
    case "$prev" in
        --outdir) out="$arg" ;;
    esac
    prev="$arg"
done
last=""
for arg in "$@"; do last="$arg"; done
printf 'hello from fake libreoffice\n' > "$out/result.txt"
printf '%s\n' "$out" > "${last%/*}/scratch.capture"
"#;
        let bin = write_fake_converter(dir.path(), "fake-libreoffice", script);
        let doc = libreoffice_attempt(bin.to_str().unwrap(), abs.to_str().unwrap(), abort_ctx())
            .await
            .expect("conversion succeeds")
            .expect("fake converter is on PATH");
        assert_eq!(doc.converter, DocConverter::LibreOffice);
        assert!(
            doc.text.contains("hello from fake libreoffice"),
            "{}",
            doc.text
        );
        let captured = std::fs::read_to_string(dir.path().join("scratch.capture"))
            .expect("fake converter captured the outdir");
        let outdir = Path::new(captured.trim());
        assert!(!outdir.exists(), "scratch outdir leaked: {}", outdir.display());
        let scratch_root = outdir.parent().expect("scratch root");
        assert!(
            !scratch_root.exists(),
            "scratch root leaked: {}",
            scratch_root.display()
        );
    }

    #[cfg(unix)]
    fn process_alive(pid: u32) -> bool {
        use nix::sys::signal::kill;
        use nix::unistd::Pid;
        kill(Pid::from_raw(pid as i32), None).is_ok()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn converter_timeout_kills_the_whole_process_group() {
        let dir = tempfile::tempdir().expect("temp dir");
        let pidfile = dir.path().join("pids");
        // Fake converter: spawn a long-lived descendant that inherits the
        // process group, record both pids, then outlast the timeout. A
        // leader-only kill leaves the descendant alive with the pipes open.
        let script = format!(
            "sleep 300 & child=$!; echo \"$$ $child\" > '{}'; sleep 300",
            pidfile.display()
        );
        let result = run_converter(
            "sh",
            &["-c".to_owned(), script, "fake-converter".to_owned()],
            Duration::from_millis(500),
            abort_ctx(),
            "sh -c fake-converter",
        )
        .await;
        let error = match result {
            Ok(_) => panic!("converter must time out, not complete"),
            Err(error) => error,
        };
        let error = error.to_string();
        assert!(error.contains("timed out after"), "got: {error}");
        let pids =
            std::fs::read_to_string(&pidfile).expect("converter wrote pids before the timeout");
        let mut parts = pids.split_whitespace();
        let leader = parts
            .next()
            .expect("leader pid")
            .parse::<u32>()
            .expect("leader pid number");
        let descendant = parts
            .next()
            .expect("descendant pid")
            .parse::<u32>()
            .expect("descendant pid number");
        // The group kill must reap the leader AND the descendant; poll past
        // the brief zombie window before init reaps the reparented child.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        for pid in [leader, descendant] {
            while process_alive(pid) {
                assert!(
                    std::time::Instant::now() < deadline,
                    "pid {pid} survived the timeout group kill"
                );
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        }
    }
}
