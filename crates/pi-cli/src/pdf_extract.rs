//! Bounded PDF text extraction shared by the Web control plane
//! (`POST /upload/pdf` in `modes::listen`) and the TUI (`@file.pdf` in
//! `file_args`).
//!
//! Raw PDF bytes never enter a `ContentBlock` and never ride the prompt
//! WebSocket JSON. Every upload is validated server-side (sanitized file
//! name, `%PDF-` magic) and reduced to bounded UTF-8 text that reuses the
//! existing prompt `message` path, plus a bounded instruction naming the
//! sanitized file.
//!
//! The only external tool is `pdftotext` (poppler-utils). It is spawned
//! directly with argv (never through a shell) under the governed
//! `DocumentConverter` family, with timeout, stdout, and stderr caps.
//! Temporary artifacts live in a request-scoped random directory that is
//! removed on drop; nothing persists server-side.

use std::{
    io,
    path::{Path, PathBuf},
    time::Duration,
};

#[cfg(test)]
use std::sync::{LazyLock, Mutex};

use http::StatusCode;
use uuid::Uuid;


/// Hard cap for one uploaded PDF's raw bytes. The listener checks
/// `Content-Length` against this before reading any body byte, and
/// [`extract_pdf`] re-checks the bytes as defense in depth.
pub(crate) const MAX_PDF_UPLOAD_BYTES: usize = 32 * 1024 * 1024;
/// Hard cap on extracted UTF-8 text that rides the prompt `message`.
pub(crate) const MAX_PDF_TEXT_BYTES: usize = 2 * 1024 * 1024;
/// Wall-clock bound for `pdftotext`.
pub(crate) const PDF_EXTRACT_TIMEOUT: Duration = Duration::from_secs(30);
/// stderr capture cap; a process whose diagnostics exceed this is treated
/// as pathological rather than stalling the request.
const MAX_PDFTOTEXT_STDERR_BYTES: usize = 64 * 1024;
/// Cap for the generated instruction/marker text.
const MAX_PDF_INSTRUCTION_CHARS: usize = 800;
/// Cap for the sanitized display stem (characters, excluding the extension).
const MAX_PDF_NAME_CHARS: usize = 120;
/// PDF header magic: `%PDF-` followed by a version (ISO 32000).
const PDF_SIGNATURE: &[u8] = b"%PDF-";
/// Sniff window for the PDF header.
const PDF_SNIFF_BYTES: usize = 1024;

/// All bounds for one PDF extraction run. Production uses
/// [`PdfLimits::default`]; tests shrink the windows so timeouts and byte
/// caps are provable without waiting out the real durations.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PdfLimits {
    pub upload_bytes: usize,
    pub text_bytes: usize,
    pub timeout: Duration,
}

impl PdfLimits {
    pub(crate) const fn default() -> Self {
        Self {
            upload_bytes: MAX_PDF_UPLOAD_BYTES,
            text_bytes: MAX_PDF_TEXT_BYTES,
            timeout: PDF_EXTRACT_TIMEOUT,
        }
    }
}

/// Classified failure of a PDF extraction run, mapped to an HTTP status by
/// the Web endpoint and rendered as bounded actionable text everywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PdfErrorKind {
    /// pdftotext is not installed / not on PATH.
    PdftotextMissing,
    /// File name extension not allowed, or content does not match `%PDF-`.
    UnsupportedType,
    /// Uploaded bytes over the cap.
    TooLarge,
    /// Extraction exceeded its wall-clock bound.
    Timeout,
    /// The file is not a decodable PDF (corrupt, encrypted, no text layer).
    InvalidMedia,
    /// Internal I/O failure (staging, work directory).
    Internal,
}

#[derive(Debug)]
pub(crate) struct PdfExtractError {
    pub kind: PdfErrorKind,
    pub message: String,
}

impl PdfExtractError {
    fn new(kind: PdfErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub(crate) fn status(&self) -> StatusCode {
        match self.kind {
            PdfErrorKind::PdftotextMissing => StatusCode::SERVICE_UNAVAILABLE,
            PdfErrorKind::UnsupportedType => StatusCode::UNSUPPORTED_MEDIA_TYPE,
            PdfErrorKind::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            PdfErrorKind::Timeout => StatusCode::GATEWAY_TIMEOUT,
            PdfErrorKind::InvalidMedia | PdfErrorKind::Internal => StatusCode::BAD_REQUEST,
        }
    }
}

/// Result of a successful extraction: sanitized identity plus bounded text
/// and the instruction/marker that prepends the prompt message.
#[derive(Clone, Debug)]
pub(crate) struct ExtractedPdf {
    /// Sanitized display name including the (lowercased) `.pdf` extension.
    pub name: String,
    pub size_bytes: usize,
    pub text: String,
    pub instruction: String,
}

/// Validate and sanitize a user-supplied PDF file name. Returns the
/// sanitized display name (bounded, safe characters, `.pdf` lowercased).
/// Returns `None` for anything without a `.pdf` extension.
///
/// The result is a DISPLAY string only — it is never used to build a
/// filesystem path. It is always the basename (last segment split on both
/// `/` and `\`), so a user-supplied absolute or relative path can never
/// project directory structure into the marker; the stem is additionally
/// scrubbed of every non-alphanumeric character except space, `-`, `_`,
/// and `.`.
pub(crate) fn sanitize_pdf_name(raw: &str) -> Option<String> {
    if raw.is_empty() || raw.len() > 4096 {
        return None;
    }
    let raw = raw.rsplit(['/', '\\']).next().unwrap_or(raw);
    let (stem, extension) = raw.rsplit_once('.')?;
    if !extension.eq_ignore_ascii_case("pdf") {
        return None;
    }
    let stem: String = stem
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, ' ' | '-' | '_' | '.') {
                character
            } else {
                ' '
            }
        })
        .collect();
    let stem = stem.split_whitespace().collect::<Vec<_>>().join(" ");
    let stem = if stem.is_empty() { "document" } else { &stem };
    let stem: String = stem
        .chars()
        .take(MAX_PDF_NAME_CHARS.saturating_sub(4))
        .collect();
    Some(format!("{stem}.pdf"))
}

/// Cheap PDF-header check against the leading magic bytes. `pdftotext`
/// remains the authority on extractability.
pub(crate) fn is_pdf_bytes(bytes: &[u8]) -> bool {
    bytes
        .get(..bytes.len().min(PDF_SNIFF_BYTES))
        .is_some_and(|prefix| prefix.starts_with(PDF_SIGNATURE))
}

/// Build the bounded instruction/marker naming the sanitized file.
pub(crate) fn build_instruction(name: &str, text_bytes: usize) -> String {
    let text = format!(
        "[PDF attachment: {name} — extracted text layer ({text_bytes} bytes); analyze the following as the document contents.]"
    );
    text.chars().take(MAX_PDF_INSTRUCTION_CHARS).collect()
}

fn truncate_utf8(text: &mut String, max_bytes: usize) {
    if text.len() <= max_bytes {
        return;
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
}


#[cfg(test)]
pub(crate) fn pdftotext_program() -> PathBuf {
    PDFTOTEXT_PROGRAM
        .lock()
        .expect("pdftotext program lock")
        .clone()
        .unwrap_or_else(|| PathBuf::from("pdftotext"))
}

#[cfg(test)]
static PDFTOTEXT_PROGRAM: LazyLock<Mutex<Option<PathBuf>>> = LazyLock::new(|| Mutex::new(None));

pub(crate) fn governed_pdftotext_program(
    process: &pi_coding::GovernedProcessContext,
) -> anyhow::Result<PathBuf> {
    #[cfg(test)]
    if let Some(program) = test_support::pdftotext_override() {
        return Ok(program);
    }
    process.pdftotext_program()
}

/// Map a governed pdftotext probe/open failure onto the same actionable
/// [`PdfExtractError`] used when spawn cannot find the converter.
pub(crate) fn pdftotext_program_error(error: &anyhow::Error) -> PdfExtractError {
    let kind = if pi_coding::is_sandbox_not_found(error) {
        io::ErrorKind::NotFound
    } else {
        io::ErrorKind::Other
    };
    pdftotext_spawn_error(kind)
}

#[cfg(test)]
static PDFTOTEXT_TEST_GUARD: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

#[cfg(test)]
struct PdftotextOverrideGuard {
    previous: Option<PathBuf>,
}

#[cfg(test)]
impl Drop for PdftotextOverrideGuard {
    fn drop(&mut self) {
        *PDFTOTEXT_PROGRAM.lock().expect("pdftotext program lock") = self.previous.take();
    }
}

#[cfg(test)]
fn set_pdftotext_override(program: PathBuf) -> PdftotextOverrideGuard {
    let mut guard = PDFTOTEXT_PROGRAM.lock().expect("pdftotext program lock");
    let previous = guard.replace(program);
    PdftotextOverrideGuard { previous }
}

/// Test-only override of the pdftotext executable for the duration of `run`.
#[cfg(test)]
pub(crate) fn with_pdftotext_program<T>(program: PathBuf, run: impl FnOnce() -> T) -> T {
    let _serial = PDFTOTEXT_TEST_GUARD
        .lock()
        .expect("serialize pdf test overrides");
    let _override = set_pdftotext_override(program);
    run()
}

/// Test-only override of the pdftotext executable for the duration of the
/// async `run` (e.g. the Web upload endpoint handler).
#[cfg(test)]
pub(crate) async fn with_pdftotext_program_async<T>(
    program: PathBuf,
    run: impl Future<Output = T>,
) -> T {
    let _serial = PDFTOTEXT_TEST_GUARD
        .lock()
        .expect("serialize pdf test overrides");
    let _override = set_pdftotext_override(program);
    run.await
}

/// Run the full validation + extraction pipeline over `pdf_bytes`.
///
/// Returns [`PdfExtractError`] with a bounded, path-scrubbed, actionable
/// message on any failure. On success every temporary artifact is already
/// deleted (the work directory is dropped before returning); the returned
/// text lives only in memory.
pub(crate) fn extract_pdf(
    process: &pi_coding::GovernedProcessContext,
    program: &Path,
    limits: PdfLimits,
    pdf_bytes: Vec<u8>,
    display_name: &str,
) -> Result<ExtractedPdf, PdfExtractError> {
    let Some(name) = sanitize_pdf_name(display_name) else {
        return Err(PdfExtractError::new(
            PdfErrorKind::UnsupportedType,
            "unsupported PDF file — attach a .pdf document",
        ));
    };
    if pdf_bytes.is_empty() {
        return Err(PdfExtractError::new(
            PdfErrorKind::InvalidMedia,
            "uploaded file is empty",
        ));
    }
    if pdf_bytes.len() > limits.upload_bytes {
        return Err(PdfExtractError::new(
            PdfErrorKind::TooLarge,
            format!(
                "PDF exceeds the {} MiB upload limit",
                limits.upload_bytes / 1024 / 1024
            ),
        ));
    }
    if !is_pdf_bytes(&pdf_bytes) {
        return Err(PdfExtractError::new(
            PdfErrorKind::UnsupportedType,
            "file content is not a PDF (missing %PDF- header)",
        ));
    }
    let size_bytes = pdf_bytes.len();
    let work_dir = WorkDir::new().map_err(|_| {
        PdfExtractError::new(
            PdfErrorKind::Internal,
            "could not create a temporary work directory",
        )
    })?;
    let pdf_path = work_dir.path.join("upload.pdf");
    std::fs::write(&pdf_path, &pdf_bytes).map_err(|_| {
        PdfExtractError::new(
            PdfErrorKind::Internal,
            "could not stage the uploaded PDF",
        )
    })?;
    drop(pdf_bytes);

    let output = run_pdftotext(process, program, work_dir.path(), &pdf_path, limits);
    if let Some(kind) = output.spawn_error {
        return Err(pdftotext_spawn_error(kind));
    }
    if output.timed_out {
        return Err(PdfExtractError::new(
            PdfErrorKind::Timeout,
            format!(
                "PDF text extraction timed out after {}s (PDF may be corrupted or encrypted)",
                limits.timeout.as_secs()
            ),
        ));
    }
    if output.cancelled {
        return Err(PdfExtractError::new(
            PdfErrorKind::Timeout,
            "PDF text extraction was cancelled",
        ));
    }
    if !matches!(output.status, Some(0)) {
        return Err(PdfExtractError::new(
            PdfErrorKind::InvalidMedia,
            format!(
                "PDF text extraction failed: {}",
                detail(&output.stderr)
            ),
        ));
    }
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    if output.stdout_capped {
        text.insert_str(
            0,
            &format!(
                "[PDF text output capped at {} bytes]\n",
                limits.text_bytes
            ),
        );
    }
    if text.trim().is_empty() {
        return Err(PdfExtractError::new(
            PdfErrorKind::InvalidMedia,
            "PDF contains no extractable text (scanned or encrypted)",
        ));
    }
    if text.len() > limits.text_bytes {
        truncate_utf8(&mut text, limits.text_bytes);
    }
    let instruction = build_instruction(&name, text.len());
    Ok(ExtractedPdf {
        name,
        size_bytes,
        text,
        instruction,
    })
}

fn pdftotext_spawn_error(kind: io::ErrorKind) -> PdfExtractError {
    if kind == io::ErrorKind::NotFound {
        PdfExtractError::new(
            PdfErrorKind::PdftotextMissing,
            "PDF text extraction requires poppler-utils (pdftotext): install it \
             (e.g. `apt install poppler-utils` or `brew install poppler`) \
             or make sure pdftotext is on PATH",
        )
    } else {
        PdfExtractError::new(
            PdfErrorKind::Internal,
            format!("failed to start pdftotext ({kind:?})"),
        )
    }
}

fn detail(stderr: &str) -> String {
    let last = stderr
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .trim();
    let mut chars = last.chars();
    let bounded: String = chars.by_ref().take(160).collect();
    if chars.next().is_some() {
        format!("{bounded}…")
    } else {
        bounded
    }
}

struct PdftotextOutput {
    spawn_error: Option<io::ErrorKind>,
    timed_out: bool,
    cancelled: bool,
    stdout_capped: bool,
    status: Option<i32>,
    stdout: Vec<u8>,
    stderr: String,
}

fn run_pdftotext(
    process: &pi_coding::GovernedProcessContext,
    program: &Path,
    work_dir: &Path,
    pdf_path: &Path,
    limits: PdfLimits,
) -> PdftotextOutput {
    let args = vec![
        "-layout".to_owned(),
        "-nopgbrk".to_owned(),
        pdf_path.to_string_lossy().into_owned(),
        "-".to_owned(),
    ];
    match process.run_blocking_document_converter(
        program,
        args,
        work_dir,
        limits.timeout,
        limits.text_bytes,
        MAX_PDFTOTEXT_STDERR_BYTES,
    ) {
        Ok(output) => PdftotextOutput {
            spawn_error: None,
            timed_out: output.timed_out,
            cancelled: output.cancelled,
            stdout_capped: output.stdout_capped,
            status: output.status,
            stdout: output.stdout,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        },
        Err(error) => {
            let kind = if pi_coding::is_sandbox_not_found(&error) {
                io::ErrorKind::NotFound
            } else {
                io::ErrorKind::Other
            };
            PdftotextOutput {
                spawn_error: Some(kind),
                timed_out: false,
                cancelled: false,
                stdout_capped: false,
                status: None,
                stdout: Vec::new(),
                stderr: String::new(),
            }
        }
    }
}

/// Request-scoped random work directory under the system temp root, removed
/// on drop. The name is generated (never user-derived), so there is no
/// symlink or traversal surface.
struct WorkDir {
    path: PathBuf,
}

impl WorkDir {
    fn new() -> io::Result<Self> {
        let path = std::env::temp_dir().join(format!("pi-pdf-{}", Uuid::new_v4()));
        std::fs::create_dir(&path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700));
        }
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    /// Fake `pdftotext` (POSIX sh) for pi-cli PDF tests. The staged PDF
    /// carries a directive after the `%PDF-` header. `VALID` prints the
    /// remainder of the file as extracted text; other modes cover empty
    /// text, failure, timeout, and huge stdout.
    pub(crate) const FAKE_SCRIPT: &str = r#"#!/bin/sh
input=""
prev=""
for arg in "$@"; do
  if [ "$prev" = "-nopgbrk" ]; then input="$arg"; fi
  prev="$arg"
done
if [ -z "$input" ]; then
  echo "fake-pdftotext: missing input" >&2
  exit 2
fi
directive=$(grep -a -m1 -E 'VALID|EMPTY|FAIL|SLOW|HUGE' "$input" 2>/dev/null)
mode=$(printf '%s' "$directive" | grep -a -m1 -o -E 'VALID|EMPTY|FAIL|SLOW|HUGE')
case "$mode" in
  VALID)
    awk 'f{print} /VALID/{f=1}' "$input"
    exit 0
    ;;
  EMPTY)
    exit 0
    ;;
  FAIL)
    echo "Command Line Error: Incorrect password" >&2
    exit 1
    ;;
  SLOW)
    sleep 5
    echo "late text"
    exit 0
    ;;
  HUGE)
    i=0
    while [ "$i" -lt 4000 ]; do
      echo "XXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX"
      i=$((i+1))
    done
    exit 0
    ;;
  *)
    echo "fake-pdftotext: unknown directive $mode" >&2
    exit 2
    ;;
esac
"#;

    pub(crate) fn fake_pdftotext() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("fake pdftotext dir");
        let script = dir.path().join("fake-pdftotext.sh");
        std::fs::write(&script, FAKE_SCRIPT).expect("write fake pdftotext");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                .expect("chmod fake pdftotext");
        }
        (dir, script)
    }

    /// Uploaded PDF bytes: `%PDF-1.7` header, then the directive on its own
    /// line, then payload text used by the VALID fake.
    pub(crate) fn pdf_bytes(directive: &str, payload: &str) -> Vec<u8> {
        let mut bytes = b"%PDF-1.7\n".to_vec();
        bytes.extend_from_slice(directive.as_bytes());
        bytes.push(b'\n');
        bytes.extend_from_slice(payload.as_bytes());
        bytes
    }

    pub(crate) fn pdftotext_override() -> Option<PathBuf> {
        PDFTOTEXT_PROGRAM
            .lock()
            .expect("pdftotext program lock")
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::test_support::{fake_pdftotext, pdf_bytes};

    fn extract(
        script: &Path,
        limits: PdfLimits,
        bytes: Vec<u8>,
        name: &str,
    ) -> Result<ExtractedPdf, PdfExtractError> {
        let _serial = PDFTOTEXT_TEST_GUARD.lock().expect("serialize pdf tests");
        crate::video_extract::test_support::with_governed_video_process(
            script.parent().expect("fake pdftotext parent"),
            |process| extract_pdf(process, script, limits, bytes, name),
        )
    }

    #[test]
    fn sanitize_accepts_pdf_and_strips_paths() {
        assert_eq!(
            sanitize_pdf_name("Report.PDF").as_deref(),
            Some("Report.pdf")
        );
        assert_eq!(
            sanitize_pdf_name("/tmp/secret/Certik audit.docx.pdf").as_deref(),
            Some("Certik audit.docx.pdf")
        );
        assert_eq!(sanitize_pdf_name("notes.txt"), None);
        assert_eq!(sanitize_pdf_name(""), None);
        assert_eq!(
            sanitize_pdf_name("weird<>name.pdf").as_deref(),
            Some("weird name.pdf")
        );
    }

    #[test]
    fn is_pdf_bytes_requires_header() {
        assert!(is_pdf_bytes(b"%PDF-1.7\n"));
        assert!(!is_pdf_bytes(b"junk %PDF-1.7\n"));
        assert!(!is_pdf_bytes(b"plain text"));
        assert!(!is_pdf_bytes(b""));
    }

    #[test]
    fn extract_valid_pdf_returns_text_and_instruction() {
        let (_dir, script) = fake_pdftotext();
        let extracted = extract(
            &script,
            PdfLimits::default(),
            pdf_bytes("VALID", "Certik findings body"),
            "Psy-ups-reports-certik-20260827.docx.pdf",
        )
        .expect("valid pdf extracts");
        assert_eq!(extracted.name, "Psy-ups-reports-certik-20260827.docx.pdf");
        assert!(extracted.text.contains("Certik findings body"));
        assert!(extracted.instruction.contains(extracted.name.as_str()));
        assert!(extracted.instruction.starts_with("[PDF attachment:"));
    }

    #[test]
    fn extract_rejects_missing_header() {
        let (_dir, script) = fake_pdftotext();
        let error = extract(
            &script,
            PdfLimits::default(),
            b"not a pdf".to_vec(),
            "doc.pdf",
        )
        .expect_err("non-pdf rejected");
        assert_eq!(error.kind, PdfErrorKind::UnsupportedType);
        assert_eq!(error.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }

    #[test]
    fn extract_rejects_wrong_extension() {
        let (_dir, script) = fake_pdftotext();
        let error = extract(
            &script,
            PdfLimits::default(),
            pdf_bytes("VALID", "body"),
            "doc.txt",
        )
        .expect_err("wrong extension rejected");
        assert_eq!(error.kind, PdfErrorKind::UnsupportedType);
    }

    #[test]
    fn extract_rejects_empty_text_layer() {
        let (_dir, script) = fake_pdftotext();
        let error = extract(
            &script,
            PdfLimits::default(),
            pdf_bytes("EMPTY", ""),
            "scan.pdf",
        )
        .expect_err("empty text rejected");
        assert_eq!(error.kind, PdfErrorKind::InvalidMedia);
        assert!(error.message.contains("no extractable text"));
    }

    #[test]
    fn extract_rejects_converter_failure() {
        let (_dir, script) = fake_pdftotext();
        let error = extract(
            &script,
            PdfLimits::default(),
            pdf_bytes("FAIL", ""),
            "locked.pdf",
        )
        .expect_err("encrypted pdf rejected");
        assert_eq!(error.kind, PdfErrorKind::InvalidMedia);
        assert!(error.message.contains("Incorrect password") || error.message.contains("failed"));
    }

    #[test]
    fn extract_rejects_oversize_upload() {
        let (_dir, script) = fake_pdftotext();
        let limits = PdfLimits {
            upload_bytes: 16,
            text_bytes: 1024,
            timeout: Duration::from_secs(5),
        };
        let error = extract(
            &script,
            limits,
            pdf_bytes("VALID", "too big payload for the tiny cap"),
            "big.pdf",
        )
        .expect_err("oversize rejected");
        assert_eq!(error.kind, PdfErrorKind::TooLarge);
        assert_eq!(error.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[test]
    fn extract_times_out_slow_converter() {
        let (_dir, script) = fake_pdftotext();
        let limits = PdfLimits {
            upload_bytes: MAX_PDF_UPLOAD_BYTES,
            text_bytes: MAX_PDF_TEXT_BYTES,
            timeout: Duration::from_millis(200),
        };
        let error = extract(
            &script,
            limits,
            pdf_bytes("SLOW", ""),
            "slow.pdf",
        )
        .expect_err("slow extract times out");
        assert_eq!(error.kind, PdfErrorKind::Timeout);
        assert_eq!(error.status(), StatusCode::GATEWAY_TIMEOUT);
    }

    #[test]
    fn truncate_utf8_cuts_before_multibyte_char() {
        let mut text = "你好世界".to_owned();
        truncate_utf8(&mut text, 4);
        assert_eq!(text, "你");
        assert!(text.is_char_boundary(text.len()));
        let mut ascii = "abcdef".to_owned();
        truncate_utf8(&mut ascii, 3);
        assert_eq!(ascii, "abc");
        let mut exact = "你好".to_owned();
        let exact_len = exact.len();
        truncate_utf8(&mut exact, exact_len);
        assert_eq!(exact, "你好");
    }

}
