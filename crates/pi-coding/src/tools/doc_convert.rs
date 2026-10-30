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

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};

use pi_agent::AbortSignal;

use super::copy_capped;
use crate::application::generation::ApplicationGeneration;
use crate::process::{
    GovernedProcessContext, GovernedProcessSpec, TrustedToolProbe, run_trusted_tool_probe,
};
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
pub(crate) async fn extract_doc_text(
    generation: Arc<ApplicationGeneration>,
    abs: &str,
    abort: AbortSignal,
) -> Result<ExtractedDoc> {
    match doc_kind(Path::new(abs)) {
        Some(DocKind::Notebook) => extract_notebook_text(generation, abs, abort).await,
        Some(DocKind::Epub) => extract_pandoc_text(generation, abs, abort).await,
        Some(DocKind::Office) => extract_office_text(generation, abs, abort).await,
        None => Err(anyhow!("unsupported document type: {abs}")),
    }
}

async fn extract_notebook_text(
    generation: Arc<ApplicationGeneration>,
    abs: &str,
    abort: AbortSignal,
) -> Result<ExtractedDoc> {
    let bin = run_trusted_tool_probe(&generation, TrustedToolProbe::Jupyter).map_err(|_| anyhow!(
        "Reading Jupyter notebooks requires jupyter nbconvert; install it (e.g. `pip install nbconvert`) or use bash: jupyter nbconvert --to script --stdout '{abs}'"
    ))?;
    extract_notebook_text_with(generation, &bin.as_path().to_string_lossy(), abs, abort).await
}

async fn extract_pandoc_text(
    generation: Arc<ApplicationGeneration>,
    abs: &str,
    abort: AbortSignal,
) -> Result<ExtractedDoc> {
    let bin = run_trusted_tool_probe(&generation, TrustedToolProbe::Pandoc).map_err(|_| anyhow!(
        "Reading EPUB files requires pandoc; install it (e.g. `apt install pandoc`) or use bash: pandoc -t plain '{abs}'"
    ))?;
    extract_pandoc_text_with(generation, &bin.as_path().to_string_lossy(), abs, abort).await
}

async fn extract_office_text(
    generation: Arc<ApplicationGeneration>,
    abs: &str,
    abort: AbortSignal,
) -> Result<ExtractedDoc> {
    let pandoc = run_trusted_tool_probe(&generation, TrustedToolProbe::Pandoc).ok();
    let libreoffice = run_trusted_tool_probe(&generation, TrustedToolProbe::LibreOffice).ok();
    extract_office_text_with(
        generation,
        pandoc
            .as_ref()
            .map(|path| path.as_path().to_string_lossy().into_owned()),
        libreoffice
            .as_ref()
            .map(|path| path.as_path().to_string_lossy().into_owned()),
        abs,
        abort,
    )
    .await
}

/// `jupyter nbconvert --to script --stdout <abs>`: code cells in execution
/// order, markdown cells as comments. The binary path comes from a trusted probe.
pub(crate) async fn extract_notebook_text_with(
    generation: Arc<ApplicationGeneration>,
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
    let run = run_converter(
        generation,
        bin,
        &args,
        DOC_EXTRACT_TIMEOUT,
        abort,
        &use_bash,
    )
    .await?;
    finish_stdout_run(bin, abs, run, DocConverter::Nbconvert, &use_bash)
}

/// `pandoc -t plain <abs>` — plain-text output keeps the extracted text
/// layout-stable for the read tool's offset/limit selection. Injectable binary
/// name for tests; used directly for EPUB (pandoc-only) and as the preferred
/// path for Office documents.
pub(crate) async fn extract_pandoc_text_with(
    generation: Arc<ApplicationGeneration>,
    bin: &str,
    abs: &str,
    abort: AbortSignal,
) -> Result<ExtractedDoc> {
    let args = vec!["-t".to_string(), "plain".to_string(), abs.to_string()];
    let use_bash = format!("pandoc -t plain '{}'", abs);
    let run = run_converter(
        generation,
        bin,
        &args,
        DOC_EXTRACT_TIMEOUT,
        abort,
        &use_bash,
    )
    .await?;
    finish_stdout_run(bin, abs, run, DocConverter::Pandoc, &use_bash)
}

/// Office text via `pandoc` (preferred) with a `libreoffice --headless
/// --convert-to txt` fallback when pandoc is not installed. A pandoc
/// *failure* (nonzero exit, timeout, abort) is reported, not masked by the
/// fallback — only a missing binary falls through. Injectable binary names for
/// tests.
pub(crate) async fn extract_office_text_with(
    generation: Arc<ApplicationGeneration>,
    pandoc_bin: Option<String>,
    libreoffice_bin: Option<String>,
    abs: &str,
    abort: AbortSignal,
) -> Result<ExtractedDoc> {
    let pandoc_args = vec!["-t".to_string(), "plain".to_string(), abs.to_string()];
    let pandoc_bash = format!("pandoc -t plain '{}'", abs);
    if let Some(pandoc_bin) = pandoc_bin.as_deref() {
        let run = run_converter(
            generation.clone(),
            pandoc_bin,
            &pandoc_args,
            DOC_EXTRACT_TIMEOUT,
            abort.clone(),
            &pandoc_bash,
        )
        .await?;
        return finish_stdout_run(pandoc_bin, abs, run, DocConverter::Pandoc, &pandoc_bash);
    }
    if let Some(libreoffice_bin) = libreoffice_bin.as_deref() {
        return libreoffice_attempt(generation, libreoffice_bin, abs, abort).await;
    }
    Err(anyhow!(
        "Reading Office documents requires pandoc or LibreOffice; install one (e.g. `apt install pandoc` or `apt install libreoffice-writer libreoffice-calc libreoffice-impress`) or use bash: {}",
        pandoc_bash
    ))
}

/// `libreoffice --headless -env:UserInstallation=<fresh profile> --convert-to
/// "txt:Text (encoded):UTF8" --outdir <scratch> <abs>`, then reads the
/// produced `.txt` back. The binary path comes from a trusted probe.
/// A fresh scratch dir per conversion isolates the user profile (parallel
/// conversions do not contend for a profile lock) and keeps the output away
/// from the source directory.
async fn libreoffice_attempt(
    generation: Arc<ApplicationGeneration>,
    bin: &str,
    abs: &str,
    abort: AbortSignal,
) -> Result<ExtractedDoc> {
    let scratch = generation
        .sandbox()
        .scratch()
        .create_converter()
        .map_err(|error| anyhow!("failed to create LibreOffice scratch dir: {error}"))?;
    let outdir = PathBuf::from("out");
    scratch
        .create_dir_all(&outdir, crate::fs::FileMode::Private)
        .map_err(|error| anyhow!("failed to create LibreOffice output dir: {error}"))?;
    let host_root = scratch.display_path(Path::new(""))?;
    let host_outdir = scratch.display_path(&outdir)?;
    let args = vec![
        "--headless".to_string(),
        format!(
            "-env:UserInstallation={}",
            file_url(&host_root.join("profile"))
        ),
        "--convert-to".to_string(),
        "txt:Text (encoded):UTF8".to_string(),
        "--outdir".to_string(),
        host_outdir.to_string_lossy().into_owned(),
        abs.to_string(),
    ];
    let use_bash = format!(
        "libreoffice --headless --convert-to txt --outdir <dir> '{}'",
        abs
    );
    let run = run_converter(
        generation,
        bin,
        &args,
        LIBREOFFICE_EXTRACT_TIMEOUT,
        abort,
        &use_bash,
    )
    .await?;
    let (out, end) = run;
    if let ConverterEnd::Exited(status) = end
        && status != 0
    {
        let status_desc = format!("exit status {status}");
        let detail = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(anyhow!(
            "libreoffice failed ({status_desc}): {detail}. Use bash: {} to inspect (the file may be corrupted or password-protected).",
            use_bash
        ));
    }
    let (bytes, capped) = read_single_txt(&scratch, &outdir)?
        .ok_or_else(|| anyhow!("libreoffice produced no output for {abs}"))?;
    scratch.destroy()?;
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
    Ok(ExtractedDoc {
        text: apply_doc_cap_notice(text, capped, bin),
        converter: DocConverter::LibreOffice,
    })
}

/// Capped stdout/stderr of a completed converter run.
struct ConverterOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    stdout_capped: bool,
}

/// How a converter run ended.
enum ConverterEnd {
    Exited(i32),
    Capped,
}

/// Runs the authorized converter with bounded output and races completion
/// against the timeout and abort signal. The common runner kills and reaps on
/// timeout, cancellation, or cap exhaustion. `use_bash` is included in the
/// timeout message as the manual-command alternative.
async fn run_converter(
    generation: Arc<ApplicationGeneration>,
    bin: &str,
    args: &[String],
    timeout: Duration,
    abort: AbortSignal,
    use_bash: &str,
) -> Result<(ConverterOutput, ConverterEnd)> {
    let cwd = Path::new(bin)
        .parent()
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| Path::new("/"));
    let output = GovernedProcessContext::new(generation, None)
        .run_with_abort(
            pi_sandbox::SpawnFamily::DocumentConverter,
            GovernedProcessSpec {
                program: PathBuf::from(bin),
                args: args.to_vec(),
                cwd: cwd.to_path_buf(),
                environment: BTreeMap::new(),
                timeout,
                stdout_bytes: DOC_EXTRACT_MAX_BYTES,
                stderr_bytes: DOC_EXTRACT_MAX_BYTES,
                #[cfg(unix)]
                mount_sources: Vec::new(),
                #[cfg(unix)]
                retained_executable: None,
                #[cfg(unix)]
                retained_working_directory: None,
            },
            Some(abort),
        )
        .await?;
    if output.timed_out {
        return Err(anyhow!(
            "{bin} timed out after {}s. Use bash: {use_bash} (the file may be corrupted or password-protected)",
            timeout.as_secs()
        ));
    }
    if output.cancelled {
        return Err(anyhow!("{bin} conversion cancelled"));
    }
    let end = if output.stdout_capped || output.stderr_capped {
        ConverterEnd::Capped
    } else {
        ConverterEnd::Exited(output.status.unwrap_or(1))
    };
    Ok((
        ConverterOutput {
            stdout: output.stdout,
            stderr: output.stderr,
            stdout_capped: output.stdout_capped,
        },
        end,
    ))
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
        ConverterEnd::Exited(status) if status != 0 => {
            let status_desc = format!("exit status {status}");
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
            &format!(
                "[{bin} output capped at {}]\n",
                format_size(DOC_EXTRACT_MAX_BYTES)
            ),
        );
    }
    text
}

/// Reads the single `.txt` file LibreOffice wrote into `dir`, capped at
/// `DOC_EXTRACT_MAX_BYTES`. `None` when no `.txt` appeared.
fn read_single_txt(
    scratch: &crate::fs::PolicyScratch,
    dir: &Path,
) -> Result<Option<(Vec<u8>, bool)>> {
    let mut entries = scratch.read_dir(dir)?;
    while let Some(entry) = entries.next()? {
        if Path::new(entry.name())
            .extension()
            .is_some_and(|extension| extension == "txt")
        {
            let bytes = match entry.into_opened() {
                crate::fs::PolicyOpenedEntry::File(file) => {
                    file.read_bounded(DOC_EXTRACT_MAX_BYTES as u64)?
                }
                crate::fs::PolicyOpenedEntry::Directory(_) => continue,
            };
            return Ok(Some((bytes, false)));
        }
    }
    Ok(None)
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
            assert_eq!(
                doc_kind(Path::new(name)),
                expected,
                "extension table for {name}"
            );
        }
    }

    #[test]
    fn governed_converter_family_is_exact() {
        assert_eq!(
            pi_sandbox::SpawnFamily::DocumentConverter.as_str(),
            "document-converter"
        );
    }

    #[test]
    fn doc_cap_notice_prefixes_and_names_converter() {
        assert_eq!(
            apply_doc_cap_notice("body".to_owned(), false, "pandoc"),
            "body"
        );
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
            DocConverter::LibreOffice
                .sed_hint(q)
                .starts_with("<(d=$(mktemp -d); libreoffice --headless"),
            "LibreOffice hint must re-run the conversion into a scratch dir"
        );
        assert!(
            DocConverter::LibreOffice
                .sed_hint(q)
                .ends_with("cat \"$d\"/*.txt)"),
            "LibreOffice hint must cat the produced txt"
        );
    }
}
