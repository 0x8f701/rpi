//! `notebook` tool: Jupyter notebook (.ipynb) read / execute / edit.
//!
//! ipynb files are plain JSON (`cells[].cell_type/source/outputs`,
//! `metadata`, `nbformat`, `nbformat_minor`), so the tool hand-parses them
//! with serde_json (no jupyter dependency) bounded at 8 MiB, and edits write
//! the document back through the same `Value` — unknown top-level fields,
//! metadata, and nbformat version are preserved exactly.
//!
//! - `read` — list the notebook's cells: index, type, an output summary for
//!   code cells, and a bounded source preview.
//! - `execute` — run code cells through a session-scoped Python kernel (the
//!   same stateful kernel implementation as the `eval` tool, see
//!   [`super::eval`]) and report per-cell stdout/stderr/result/errors. Cell
//!   outputs are written back into the .ipynb only when `write` is true;
//!   otherwise the file is left untouched. A cell timeout kills the kernel;
//!   the remaining cells are skipped and the next call respawns it.
//! - `edit` — append a markdown/code/raw cell, writing the document back
//!   while preserving unknown fields.
//!
//! Bounds: 8 MiB file cap, 200-cell read preview, 200-char source preview,
//! 64 KiB per kernel stream (from the eval kernel), and a 32 KiB rendered
//! result cap. Every rendered result runs through the secret redactor.
//!
//! # Capabilities
//!
//! The tool computes its required capability per action before dispatch:
//! `read` → Read, `execute` → Exec, `edit` → Write. Actions whose required
//! capability is not granted are refused before any file access, so a
//! read-only role can read notebooks without gaining edit/execute and an
//! exec role can execute without gaining read/edit. The declared tool
//! capability stays Exec (the dominant action) for harness-level filtering;
//! the per-action check is the action-level enforcement documented here.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use tokio::sync::Mutex as AsyncMutex;

use pi_agent::{AbortSignal, AgentTool, AgentToolResult, ToolCallContext, ToolCapability};
use pi_ai::Schema;

use crate::redact::redact_secrets;
use crate::truncate::truncate_tail;

use super::eval::{CellError, CellResult, EvalErrorKind, PythonKernel, cell_timeout};
use super::{arg_bool, arg_int, arg_str, check_aborted, s_array, s_boolean, s_number, s_object, s_string, text_result};

/// `notebook` refuses files larger than this. Enforced from metadata BEFORE
/// any read, so an oversized (even sparse) file is rejected without touching
/// its contents.
const MAX_NOTEBOOK_BYTES: u64 = 8 * 1024 * 1024; // 8 MiB
/// Cap on rendered read output (matches the debug/mcp result cap).
const OUTPUT_MAX_BYTES: usize = 32 * 1024;
/// Cap on cells shown by `read` (a huge notebook stays readable).
const MAX_RENDER_CELLS: usize = 200;
/// Cap on the per-cell source preview shown by `read`.
const CELL_PREVIEW_CHARS: usize = 200;

/// The implemented actions, listed in the schema and in validation errors.
const ACTIONS: &str = "read, execute, edit";

/// Maps a notebook action to the tool capability its file operations need:
/// `read` → Read (arbitrary file reads), `execute` → Exec (runs code through
/// the kernel), `edit` → Write (rewrites the .ipynb). The tool computes this
/// BEFORE dispatch and refuses actions outside the granted capability set,
/// so a read-only role can read notebooks without gaining edit/execute, and
/// an exec role can execute without gaining read/edit.
fn action_capability(action: &str) -> Option<ToolCapability> {
    match action {
        "read" => Some(ToolCapability::Read),
        "execute" => Some(ToolCapability::Exec),
        "edit" => Some(ToolCapability::Write),
        _ => None,
    }
}

fn capability_name(capability: ToolCapability) -> &'static str {
    match capability {
        ToolCapability::Read => "read",
        ToolCapability::Write => "write",
        ToolCapability::Exec => "exec",
    }
}

/// Builds the `notebook` tool rooted at `cwd` (session-scoped Python kernel
/// for `execute`). Grants the full capability set (read/execute/edit); the
/// per-action capability check still applies per call.
pub(crate) fn notebook_tool(cwd: &str) -> AgentTool {
    notebook_tool_with_capabilities(cwd, None, all_capabilities())
}

/// [`notebook_tool`] with an explicit python interpreter override (tests).
fn notebook_tool_with_python(cwd: &str, python_override: Option<&str>) -> AgentTool {
    notebook_tool_with_capabilities(cwd, python_override, all_capabilities())
}

/// [`notebook_tool_with_python`] with an explicit granted capability set:
/// actions whose required capability is not granted are refused before any
/// dispatch (tests exercise the read-only / exec-only roles this way).
fn notebook_tool_with_capabilities(
    cwd: &str,
    python_override: Option<&str>,
    granted: Vec<ToolCapability>,
) -> AgentTool {
    let python_override = python_override.map(String::from);
    let granted = Arc::new(granted);
    let description = format!(
        "Read, execute, and edit Jupyter notebooks (.ipynb). Actions: {ACTIONS}. \
         `read path=...` lists the notebook's cells (index, type, output summary, bounded \
         source preview; files larger than 8 MiB are refused). \
         `execute path=... [cell=N | all] [write=true]` runs code cells through a \
         session-scoped Python kernel and reports per-cell stdout/stderr/result/errors \
         (outputs are written back into the .ipynb only when `write` is true; a cell \
         timeout kills the kernel and skips the remaining cells). \
         `edit path=... cell_type=markdown|code|raw source=...` appends a cell, preserving \
         unknown notebook fields. `timeout` bounds each cell (default 30s)."
    );
    let source_schema = Schema {
        any_of: vec![
            s_string("Cell source text"),
            s_array(s_string("Source line"), "Cell source as an array of lines"),
        ],
        description: Some("Cell source text (string or array of lines)".to_owned()),
        ..Default::default()
    };
    let params = s_object(
        vec![
            (
                "action",
                s_string(&format!("Notebook action to run. One of: {ACTIONS}")),
            ),
            (
                "path",
                s_string("Path to the .ipynb notebook (resolved against the session cwd)"),
            ),
            (
                "cell",
                s_number("0-based cell index for execute (default: all code cells)"),
            ),
            (
                "write",
                s_boolean("Persist cell outputs back into the .ipynb (execute; default false)"),
            ),
            (
                "cell_type",
                s_string("Cell type for edit: markdown, code, or raw (default markdown)"),
            ),
            ("source", source_schema),
            (
                "timeout",
                s_number("Per-cell timeout in seconds (execute; default 30, min 1, max 300)"),
            ),
        ],
        vec!["action", "path"],
    );
    let kernels = NotebookKernels::default();
    let cwd = cwd.to_owned();
    AgentTool::new("notebook", description, params, move |ctx: ToolCallContext| {
        let kernels = kernels.clone();
        let cwd = cwd.clone();
        let python_override = python_override.clone();
        let granted = granted.clone();
        async move {
            run_notebook(
                &kernels,
                &cwd,
                python_override.as_deref(),
                granted,
                ctx.arguments,
                ctx.abort,
            )
            .await
        }
    })
    .with_capability(ToolCapability::Exec)
    .with_prompt_guidelines(vec![
        "Notebook cells share one Python kernel per session: state set by an earlier cell is visible to later cells; a timeout kills the kernel and the rest of the run is skipped.".to_string(),
        "Execute does not modify the .ipynb unless write=true — pass write=true when the run must persist outputs back into the notebook.".to_string(),
        "Edits append a cell and preserve every other field of the notebook document (metadata, nbformat, other cells).".to_string(),
    ])
}

/// Session-scoped Python kernel slot for `execute` (one kernel per tool
/// instance, lazily spawned, killed by timeout/error — mirrors the `eval`
/// registry).
#[derive(Clone, Default)]
struct NotebookKernels {
    inner: Arc<AsyncMutex<Option<PythonKernel>>>,
}

/// Entry point: validates the action, applies the action-aware capability
/// gate (read → Read, execute → Exec, edit → Write) BEFORE any dispatch,
/// and runs the action.
async fn run_notebook(
    kernels: &NotebookKernels,
    cwd: &str,
    python_override: Option<&str>,
    granted: Arc<Vec<ToolCapability>>,
    args: Value,
    abort: AbortSignal,
) -> Result<AgentToolResult> {
    check_aborted(&abort)?;
    let action = arg_str(&args, "action").trim().to_ascii_lowercase();
    let path = arg_str(&args, "path");
    if path.trim().is_empty() {
        bail!("notebook requires a `path` (.ipynb file)");
    }
    let Some(required) = action_capability(&action) else {
        if action.is_empty() {
            bail!("notebook action is required (one of: {ACTIONS})");
        }
        bail!("unknown notebook action `{action}` (one of: {ACTIONS})");
    };
    if !granted.contains(&required) {
        bail!(
            "notebook `{action}` requires the {} capability, which is not granted to this \
             session",
            capability_name(required)
        );
    }
    match action.as_str() {
        "read" => run_notebook_read(cwd, &args),
        "execute" => run_notebook_execute(kernels, cwd, python_override, &args, &abort).await,
        "edit" => run_notebook_edit(cwd, &args),
        _ => unreachable!("action_capability validated the action"),
    }
}

/// The full capability set a standalone notebook tool is granted.
fn all_capabilities() -> Vec<ToolCapability> {
    vec![
        ToolCapability::Read,
        ToolCapability::Write,
        ToolCapability::Exec,
    ]
}

// ---------------------------------------------------------------------------
// Parsing / writing (ipynb is JSON: cells, metadata, nbformat)
// ---------------------------------------------------------------------------

/// Resolves `path` against the session `cwd` (absolute paths pass through).
fn resolve_path(cwd: &str, path: &str) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        Path::new(cwd).join(p)
    }
}

/// Reads and parses a notebook: 8 MiB size gate from metadata before any
/// read, JSON parse, and a `cells` array sanity check. Returns the absolute
/// path (for writes) and the parsed document.
fn read_notebook(cwd: &str, path: &str) -> Result<(PathBuf, Value)> {
    let abs = resolve_path(cwd, path);
    let metadata = std::fs::metadata(&abs).with_context(|| format!("reading notebook {path}"))?;
    if metadata.len() > MAX_NOTEBOOK_BYTES {
        bail!(
            "notebook {path} is {} bytes, exceeding the 8 MiB bound",
            metadata.len()
        );
    }
    let text = std::fs::read_to_string(&abs).with_context(|| format!("reading notebook {path}"))?;
    let doc: Value = serde_json::from_str(&text)
        .with_context(|| format!("{path} is not a valid Jupyter notebook (invalid JSON)"))?;
    if !doc.get("cells").and_then(Value::as_array).is_some() {
        bail!("{path} is not a Jupyter notebook: missing `cells` array");
    }
    Ok((abs, doc))
}

/// The cell type (`code`/`markdown`/`raw`), defaulting to `code`.
fn cell_type_of(cell: &Value) -> &str {
    cell.get("cell_type").and_then(Value::as_str).unwrap_or("code")
}

/// The cell source, joined from the string-or-array-of-lines nbformat form.
fn cell_source(cell: &Value) -> String {
    match cell.get("source") {
        Some(Value::String(source)) => source.clone(),
        Some(Value::Array(lines)) => lines.iter().filter_map(Value::as_str).collect(),
        _ => String::new(),
    }
}

/// Writes the document back preserving every field (the `Value` is the whole
/// notebook, so unknown top-level fields survive).
fn write_notebook(abs: &Path, doc: &Value) -> Result<()> {
    let mut text = serde_json::to_string_pretty(doc).context("serializing notebook")?;
    text.push('\n');
    std::fs::write(abs, text).with_context(|| format!("writing notebook {}", abs.display()))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// read
// ---------------------------------------------------------------------------

fn run_notebook_read(cwd: &str, args: &Value) -> Result<AgentToolResult> {
    let path = arg_str(args, "path");
    let (_, doc) = read_notebook(cwd, &path)?;
    let cells = doc
        .get("cells")
        .and_then(Value::as_array)
        .context("notebook has no cells")?;
    let nbformat = doc.get("nbformat").and_then(Value::as_u64).unwrap_or(4);
    let minor = doc.get("nbformat_minor").and_then(Value::as_u64).unwrap_or(0);
    let mut text = format!(
        "notebook: {path} — nbformat {nbformat}.{minor}, {} cells\n",
        cells.len()
    );
    if let Some(metadata) = doc.get("metadata").and_then(Value::as_object)
        && !metadata.is_empty()
    {
        text.push_str(&format!("metadata: {}\n", compact_json(&doc["metadata"])));
    }
    let shown = cells.len().min(MAX_RENDER_CELLS);
    for (index, cell) in cells.iter().take(shown).enumerate() {
        match cell_type_of(cell) {
            "code" => {
                let outputs = cell
                    .get("outputs")
                    .and_then(Value::as_array)
                    .map(Vec::len)
                    .unwrap_or(0);
                let has_error = cell
                    .get("outputs")
                    .and_then(Value::as_array)
                    .is_some_and(|outputs| {
                        outputs
                            .iter()
                            .any(|o| o.get("output_type").and_then(Value::as_str) == Some("error"))
                    });
                text.push_str(&format!(
                    "[{index}] code ({outputs} outputs{})\n",
                    if has_error { ", includes error" } else { "" }
                ));
            }
            "markdown" => text.push_str(&format!("[{index}] markdown\n")),
            "raw" => text.push_str(&format!("[{index}] raw\n")),
            other => text.push_str(&format!("[{index}] {other}\n")),
        }
        let preview = preview_source(&cell_source(cell));
        if !preview.is_empty() {
            text.push_str(&format!("    {}\n", preview.replace('\n', "\n    ")));
        }
    }
    if cells.len() > shown {
        text.push_str(&format!(
            "... {} more cells omitted\n",
            cells.len() - shown
        ));
    }
    Ok(text_result(bounded(&redact_secrets(&text))))
}

/// First [`CELL_PREVIEW_CHARS`] characters of a cell source, single-line
/// safe (newlines are preserved for indented previews).
fn preview_source(source: &str) -> String {
    let mut preview: String = source.chars().take(CELL_PREVIEW_CHARS).collect();
    if source.chars().count() > CELL_PREVIEW_CHARS {
        preview.push_str("...");
    }
    preview
}

/// Compact single-line rendering of the notebook metadata (bounded).
fn compact_json(value: &Value) -> String {
    match serde_json::to_string(value) {
        Ok(text) if text.len() <= 400 => text,
        _ => "[truncated metadata]".to_owned(),
    }
}

// ---------------------------------------------------------------------------
// execute
// ---------------------------------------------------------------------------

/// Runs the selected code cells through the session Python kernel and reports
/// per-cell results. Outputs are persisted only when `write` is true. A cell
/// timeout kills the kernel (respawned on the next call) and skips the rest
/// of the run.
async fn run_notebook_execute(
    kernels: &NotebookKernels,
    cwd: &str,
    python_override: Option<&str>,
    args: &Value,
    abort: &AbortSignal,
) -> Result<AgentToolResult> {
    let path = arg_str(args, "path");
    let timeout = cell_timeout(args)?;
    let (abs, mut doc) = read_notebook(cwd, &path)?;
    let write = arg_bool(args, "write");
    let cell_count = doc
        .get("cells")
        .and_then(Value::as_array)
        .map(Vec::len)
        .context("notebook has no cells")?;
    let indices: Vec<usize> = match arg_int(args, "cell")? {
        Some(index) => {
            if index < 0 || index as usize >= cell_count {
                bail!(
                    "notebook execute `cell` {index} is out of range (notebook has {cell_count} cells)"
                );
            }
            vec![index as usize]
        }
        None => (0..cell_count)
            .filter(|index| cell_type_of(&doc["cells"][*index]) == "code")
            .collect(),
    };
    if indices.is_empty() {
        return Ok(text_result(format!(
            "notebook: {path} — no code cells to execute"
        )));
    }

    let mut slot = kernels.inner.lock().await;
    let mut kernel = match slot.take() {
        Some(kernel) => kernel,
        None => PythonKernel::spawn_with(python_override, cwd).await?,
    };
    let mut dead = false;
    let mut report = String::new();
    let mut wrote = false;
    let mut execution_count: u64 = 1;
    for index in &indices {
        let code = cell_source(&doc["cells"][*index]);
        if code.trim().is_empty() {
            report.push_str(&format!("[{index}] code cell is empty — skipped\n"));
            continue;
        }
        let cell = match kernel.eval(&code, timeout, abort).await {
            Ok(cell) => cell,
            Err(error) => {
                dead = true;
                bail!("notebook execute failed in cell {index}: {error}");
            }
        };
        report.push_str(&format!("[{index}] {}\n", render_notebook_cell(&cell)));
        if write && !cell.is_timeout() {
            if let Some(cell_value) = doc
                .get_mut("cells")
                .and_then(Value::as_array_mut)
                .and_then(|cells| cells.get_mut(*index))
            {
                cell_value["outputs"] = ipynb_outputs(&cell);
                cell_value["execution_count"] = json!(execution_count);
                execution_count += 1;
                wrote = true;
            }
        }
        if cell.is_timeout() {
            dead = true;
            report.push_str(
                "    (kernel timed out — respawned on the next call; remaining cells skipped)\n",
            );
            break;
        }
    }
    if !dead {
        *slot = Some(kernel);
    }
    if write && wrote {
        write_notebook(&abs, &doc)?;
        report.push_str(&format!("wrote outputs to {path}\n"));
    }
    Ok(text_result(bounded(&redact_secrets(&report))))
}

/// Renders one cell outcome as a single indented block for the execute
/// report.
fn render_notebook_cell(cell: &CellResult) -> String {
    let mut text = match &cell.error {
        None => "ok".to_owned(),
        Some(error) => match error.kind {
            EvalErrorKind::Syntax => "syntax error".to_owned(),
            EvalErrorKind::Runtime => "runtime error".to_owned(),
            EvalErrorKind::Timeout => "timeout".to_owned(),
        },
    };
    if !cell.stdout.is_empty() {
        text.push_str(&format!("\nstdout: {}", cell.stdout.trim_end()));
    }
    if !cell.stderr.is_empty() {
        text.push_str(&format!("\nstderr: {}", cell.stderr.trim_end()));
    }
    if let Some(result) = &cell.result {
        text.push_str(&format!("\nresult: {result}"));
    }
    if let Some(error) = &cell.error {
        text.push_str(&format!("\nerror:\n{}", error.text.trim_end()));
    }
    text
}

/// Converts a [`CellResult`] into an ipynb `outputs` array (stream /
/// execute_result / error entries).
fn ipynb_outputs(cell: &CellResult) -> Value {
    let mut outputs = Vec::new();
    if !cell.stdout.is_empty() {
        outputs.push(json!({
            "output_type": "stream",
            "name": "stdout",
            "text": cell.stdout,
        }));
    }
    if !cell.stderr.is_empty() {
        outputs.push(json!({
            "output_type": "stream",
            "name": "stderr",
            "text": cell.stderr,
        }));
    }
    if let Some(result) = &cell.result {
        outputs.push(json!({
            "output_type": "execute_result",
            "execution_count": null,
            "metadata": {},
            "data": { "text/plain": result },
        }));
    }
    if let Some(error) = &cell.error {
        let ename = match error.kind {
            EvalErrorKind::Syntax => "SyntaxError",
            EvalErrorKind::Runtime => "RuntimeError",
            EvalErrorKind::Timeout => "TimeoutError",
        };
        outputs.push(json!({
            "output_type": "error",
            "ename": ename,
            "evalue": error.text,
            "traceback": [error.text],
        }));
    }
    Value::Array(outputs)
}

// ---------------------------------------------------------------------------
// edit
// ---------------------------------------------------------------------------

/// Appends a markdown/code/raw cell, writing the document back while
/// preserving unknown fields.
fn run_notebook_edit(cwd: &str, args: &Value) -> Result<AgentToolResult> {
    let path = arg_str(args, "path");
    let cell_type = arg_str(args, "cell_type").to_ascii_lowercase();
    if cell_type.is_empty() {
        bail!("notebook edit requires a `cell_type` (markdown, code, or raw)");
    }
    if !["markdown", "code", "raw"].contains(&cell_type.as_str()) {
        bail!(
            "notebook edit `cell_type` must be markdown, code, or raw (got `{cell_type}`)"
        );
    }
    let source = match args.get("source") {
        Some(Value::String(source)) => source.clone(),
        Some(Value::Array(lines)) => lines.iter().filter_map(Value::as_str).collect(),
        _ => String::new(),
    };
    if source.trim().is_empty() {
        bail!("notebook edit requires non-empty `source`");
    }
    let (abs, mut doc) = read_notebook(cwd, &path)?;
    let new_len = {
        let cells = doc
            .get_mut("cells")
            .and_then(Value::as_array_mut)
            .context("notebook has no cells")?;
        let mut cell = json!({
            "cell_type": cell_type,
            "metadata": {},
            "source": source_to_lines(&source),
        });
        if cell_type == "code" {
            cell["outputs"] = json!([]);
            cell["execution_count"] = Value::Null;
        }
        cells.push(cell);
        cells.len()
    };
    write_notebook(&abs, &doc)?;
    Ok(text_result(format!(
        "appended {cell_type} cell to {path} (now {new_len} cells)"
    )))
}

/// Converts free-form source text to the nbformat line-array convention (each
/// line carries its trailing newline except the last).
fn source_to_lines(source: &str) -> Value {
    let lines: Vec<String> = source.split_inclusive('\n').map(String::from).collect();
    let lines = if lines.is_empty() {
        vec![String::new()]
    } else {
        lines
    };
    Value::Array(lines.into_iter().map(Value::String).collect())
}

/// Bounds a rendered result to [`OUTPUT_MAX_BYTES`] (tail).
fn bounded(text: &str) -> String {
    truncate_tail(text, 500, OUTPUT_MAX_BYTES).content
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::Arc;

    use serde_json::json;
    use tempfile::tempdir;

    use pi_agent::{AbortController, AgentToolResult, ToolCallContext, ToolUpdateFn};

    fn noop_update() -> ToolUpdateFn {
        Arc::new(|_r: AgentToolResult| {})
    }

    fn make_ctx(args: Value) -> ToolCallContext {
        let (_ctrl, abort) = AbortController::new();
        std::mem::forget(_ctrl);
        ToolCallContext {
            tool_call_id: "notebook-test".to_string(),
            arguments: args,
            on_update: noop_update(),
            abort,
            model: None,
        }
    }

    fn text_of(res: &AgentToolResult) -> String {
        match res.content.first() {
            Some(pi_ai::ContentBlock::Text { text, .. }) => text.clone(),
            _ => String::new(),
        }
    }

    fn python3_available() -> bool {
        std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_ok_and(|out| out.status.success())
    }

    /// A minimal nbformat-4 notebook with two code cells and one markdown
    /// cell, plus unknown top-level fields that edits must preserve.
    fn fixture_notebook() -> Value {
        json!({
            "cells": [
                {
                    "cell_type": "code",
                    "execution_count": null,
                    "metadata": {},
                    "outputs": [],
                    "source": ["a = 21\n"]
                },
                {
                    "cell_type": "markdown",
                    "metadata": {},
                    "source": ["# Title\n", "Some *text*.\n"]
                },
                {
                    "cell_type": "code",
                    "execution_count": null,
                    "metadata": {},
                    "outputs": [],
                    "source": ["print('n =', a)\n", "a * 2\n"]
                }
            ],
            "metadata": {
                "kernelspec": { "display_name": "Python 3", "language": "python", "name": "python3" },
                "language_info": { "name": "python", "version": "3.11" }
            },
            "nbformat": 4,
            "nbformat_minor": 5,
            "custom_top_level_field": { "keep": "me" }
        })
    }

    fn write_fixture(dir: &std::path::Path) -> String {
        let path = dir.join("notes.ipynb");
        fs::write(&path, serde_json::to_string_pretty(&fixture_notebook()).unwrap()).unwrap();
        path.to_string_lossy().into_owned()
    }

    #[tokio::test]
    async fn notebook_read_lists_cells_with_output_summary() {
        let dir = tempdir().expect("tempdir");
        let path = write_fixture(dir.path());
        let tool = notebook_tool(&dir.path().to_string_lossy());
        let result = (tool.execute)(make_ctx(json!({ "action": "read", "path": path })))
            .await
            .expect("read");
        let text = text_of(&result);
        assert!(text.contains("nbformat 4.5"), "{text}");
        assert!(text.contains("3 cells"), "{text}");
        assert!(text.contains("[0] code (0 outputs)"), "{text}");
        assert!(text.contains("[1] markdown"), "{text}");
        assert!(text.contains("[2] code (0 outputs)"), "{text}");
        assert!(text.contains("a = 21"), "{text}");
        assert!(text.contains("# Title"), "{text}");
    }

    #[tokio::test]
    async fn notebook_execute_runs_code_cells_without_writing() {
        if !python3_available() {
            return;
        }
        let dir = tempdir().expect("tempdir");
        let path = write_fixture(dir.path());
        let tool = notebook_tool(&dir.path().to_string_lossy());
        let result = (tool.execute)(make_ctx(json!({ "action": "execute", "path": path })))
            .await
            .expect("execute");
        let text = text_of(&result);
        assert!(text.contains("[0] ok"), "{text}");
        assert!(text.contains("[2] ok"), "{text}");
        assert!(text.contains("result: 42"), "{text}");
        assert!(text.contains("n = 21"), "{text}");
        // Without write=true the file is untouched.
        let after = fs::read_to_string(&dir.path().join("notes.ipynb")).unwrap();
        assert_eq!(after, serde_json::to_string_pretty(&fixture_notebook()).unwrap());
    }

    #[tokio::test]
    async fn notebook_execute_write_persists_outputs_preserving_fields() {
        if !python3_available() {
            return;
        }
        let dir = tempdir().expect("tempdir");
        let path = write_fixture(dir.path());
        let tool = notebook_tool(&dir.path().to_string_lossy());
        let result = (tool.execute)(make_ctx(json!({
            "action": "execute",
            "path": path,
            "write": true,
        })))
        .await
        .expect("execute with write");
        let text = text_of(&result);
        assert!(text.contains("wrote outputs to"), "{text}");
        let after: Value =
            serde_json::from_str(&fs::read_to_string(&dir.path().join("notes.ipynb")).unwrap())
                .unwrap();
        // Unknown top-level fields, metadata, and nbformat survive.
        assert_eq!(after["custom_top_level_field"], json!({ "keep": "me" }));
        assert_eq!(after["nbformat"], 4);
        assert_eq!(after["metadata"]["kernelspec"]["name"], "python3");
        // Code cells now carry outputs + execution counts; the markdown cell
        // is untouched. Cell 0 is a bare assignment (no expression result),
        // so it gets an execution count but zero outputs — matching Jupyter.
        let outputs = after["cells"][0]["outputs"].as_array().unwrap();
        assert_eq!(outputs.len(), 0, "{outputs:?}");
        assert_eq!(after["cells"][0]["execution_count"], 1);
        let outputs = after["cells"][2]["outputs"].as_array().unwrap();
        assert_eq!(outputs.len(), 2, "{outputs:?}");
        assert_eq!(outputs[0]["output_type"], "stream");
        assert_eq!(outputs[0]["name"], "stdout");
        assert_eq!(outputs[1]["output_type"], "execute_result");
        assert_eq!(outputs[1]["data"]["text/plain"], "42");
        assert_eq!(after["cells"][2]["execution_count"], 2);
        assert!(after["cells"][1]["outputs"].is_null());
    }

    #[tokio::test]
    async fn notebook_execute_reports_errors_per_cell() {
        if !python3_available() {
            return;
        }
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("broken.ipynb");
        fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "cells": [
                    { "cell_type": "code", "execution_count": null, "metadata": {}, "outputs": [], "source": ["1 / 0\n"] },
                    { "cell_type": "code", "execution_count": null, "metadata": {}, "outputs": [], "source": ["2 + 2\n"] }
                ],
                "metadata": {},
                "nbformat": 4,
                "nbformat_minor": 5
            }))
            .unwrap(),
        )
        .unwrap();
        let tool = notebook_tool(&dir.path().to_string_lossy());
        let result = (tool.execute)(make_ctx(json!({ "action": "execute", "path": path })))
            .await
            .expect("execute");
        let text = text_of(&result);
        assert!(text.contains("[0] runtime error"), "{text}");
        assert!(text.contains("ZeroDivisionError"), "{text}");
        // The kernel survives a runtime error: the next cell runs.
        assert!(text.contains("[1] ok"), "{text}");
        assert!(text.contains("result: 4"), "{text}");
    }

    #[tokio::test]
    async fn notebook_edit_appends_cell_preserving_unknown_fields() {
        let dir = tempdir().expect("tempdir");
        let path = write_fixture(dir.path());
        let tool = notebook_tool(&dir.path().to_string_lossy());
        let result = (tool.execute)(make_ctx(json!({
            "action": "edit",
            "path": path,
            "cell_type": "markdown",
            "source": "## Added\n",
        })))
        .await
        .expect("edit");
        assert!(text_of(&result).contains("appended markdown cell"), "{}", text_of(&result));
        let after: Value =
            serde_json::from_str(&fs::read_to_string(&dir.path().join("notes.ipynb")).unwrap())
                .unwrap();
        assert_eq!(after["nbformat"], 4);
        assert_eq!(after["nbformat_minor"], 5);
        assert_eq!(after["metadata"]["kernelspec"]["name"], "python3");
        assert_eq!(after["custom_top_level_field"], json!({ "keep": "me" }));
        let cells = after["cells"].as_array().unwrap();
        assert_eq!(cells.len(), 4);
        assert_eq!(cells[3]["cell_type"], "markdown");
        assert_eq!(cells[3]["source"], json!(["## Added\n"]));
        // The original cells are untouched.
        assert_eq!(cells[1]["source"], json!(["# Title\n", "Some *text*.\n"]));
    }

    #[tokio::test]
    async fn notebook_edit_code_cell_carries_outputs_and_execution_count() {
        let dir = tempdir().expect("tempdir");
        let path = write_fixture(dir.path());
        let tool = notebook_tool(&dir.path().to_string_lossy());
        let result = (tool.execute)(make_ctx(json!({
            "action": "edit",
            "path": path,
            "cell_type": "code",
            "source": ["print(1)\n"],
        })))
        .await
        .expect("edit");
        assert!(text_of(&result).contains("appended code cell"), "{}", text_of(&result));
        let after: Value =
            serde_json::from_str(&fs::read_to_string(&dir.path().join("notes.ipynb")).unwrap())
                .unwrap();
        let cell = &after["cells"][3];
        assert_eq!(cell["cell_type"], "code");
        assert_eq!(cell["source"], json!(["print(1)\n"]));
        assert_eq!(cell["outputs"], json!([]));
        assert!(cell["execution_count"].is_null());
    }

    #[tokio::test]
    async fn notebook_read_rejects_oversized_files() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("huge.ipynb");
        // A sparse file larger than the 8 MiB bound is rejected by metadata
        // before any read.
        let file = fs::File::create(&path).unwrap();
        file.set_len(MAX_NOTEBOOK_BYTES + 1).unwrap();
        let tool = notebook_tool(&dir.path().to_string_lossy());
        let error = (tool.execute)(make_ctx(json!({ "action": "read", "path": path })))
            .await
            .expect_err("oversized notebook refused");
        assert!(error.to_string().contains("8 MiB"), "{error}");
    }

    #[tokio::test]
    async fn notebook_read_rejects_non_notebook_json() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("not-notebook.ipynb");
        fs::write(&path, r#"{"hello": "world"}"#).unwrap();
        let tool = notebook_tool(&dir.path().to_string_lossy());
        let error = (tool.execute)(make_ctx(json!({ "action": "read", "path": path })))
            .await
            .expect_err("missing cells rejected");
        assert!(error.to_string().contains("cells"), "{error}");
    }

    #[tokio::test]
    async fn notebook_execute_missing_python_is_actionable() {
        let dir = tempdir().expect("tempdir");
        let path = write_fixture(dir.path());
        let tool = notebook_tool_with_python(&dir.path().to_string_lossy(), Some("python3-definitely-missing"));
        let error = (tool.execute)(make_ctx(json!({ "action": "execute", "path": path })))
            .await
            .expect_err("missing python must fail actionably");
        let text = format!("{error:#}");
        assert!(text.contains("python3"), "{text}");
        assert!(text.contains("install Python 3"), "{text}");
    }

    #[tokio::test]
    async fn notebook_requires_action_and_path() {
        let dir = tempdir().expect("tempdir");
        let tool = notebook_tool(&dir.path().to_string_lossy());
        let missing_action = (tool.execute)(make_ctx(json!({ "path": "x.ipynb" })))
            .await
            .expect_err("action required");
        assert!(missing_action.to_string().contains("action"), "{missing_action}");
        let bad_action = (tool.execute)(make_ctx(json!({ "action": "delete", "path": "x.ipynb" })))
            .await
            .expect_err("unknown action");
        assert!(bad_action.to_string().contains("delete"), "{bad_action}");
    }

    #[test]
    fn action_capability_maps_actions_to_tool_capabilities() {
        assert_eq!(action_capability("read"), Some(ToolCapability::Read));
        assert_eq!(action_capability("execute"), Some(ToolCapability::Exec));
        assert_eq!(action_capability("edit"), Some(ToolCapability::Write));
        // Unknown or empty actions have no capability (validation errors).
        assert_eq!(action_capability("delete"), None);
        assert_eq!(action_capability(""), None);
    }

    #[tokio::test]
    async fn read_only_role_can_read_but_not_edit_or_execute() {
        let dir = tempdir().expect("tempdir");
        let path = write_fixture(dir.path());
        let read_only = notebook_tool_with_capabilities(
            &dir.path().to_string_lossy(),
            None,
            vec![ToolCapability::Read],
        );
        // Read is allowed for the read-only role.
        let read = (read_only.execute)(make_ctx(json!({ "action": "read", "path": path })))
            .await
            .expect("read allowed");
        assert!(text_of(&read).contains("3 cells"), "{}", text_of(&read));
        // Edit (Write) and execute (Exec) are refused BEFORE any dispatch —
        // the file is untouched and no kernel is spawned.
        let edit = (read_only.execute)(make_ctx(json!({
            "action": "edit",
            "path": path,
            "cell_type": "markdown",
            "source": "## Sneaky\n",
        })))
        .await
        .expect_err("edit refused for read-only role");
        let message = edit.to_string();
        assert!(message.contains("write"), "{message}");
        assert!(message.contains("capability"), "{message}");
        let exec = (read_only.execute)(make_ctx(json!({ "action": "execute", "path": path })))
            .await
            .expect_err("execute refused for read-only role");
        assert!(exec.to_string().contains("exec"), "{exec}");
        let after = fs::read_to_string(&dir.path().join("notes.ipynb")).unwrap();
        assert_eq!(
            after,
            serde_json::to_string_pretty(&fixture_notebook()).unwrap(),
            "refused edit must leave the notebook untouched"
        );
    }

    #[tokio::test]
    async fn exec_role_can_execute_but_not_read_or_edit() {
        let dir = tempdir().expect("tempdir");
        let path = write_fixture(dir.path());
        let exec_only = notebook_tool_with_capabilities(
            &dir.path().to_string_lossy(),
            None,
            vec![ToolCapability::Exec],
        );
        // Read and edit are refused for the exec role.
        let read = (exec_only.execute)(make_ctx(json!({ "action": "read", "path": path })))
            .await
            .expect_err("read refused for exec role");
        assert!(read.to_string().contains("read"), "{read}");
        let edit = (exec_only.execute)(make_ctx(json!({
            "action": "edit",
            "path": path,
            "cell_type": "markdown",
            "source": "## Sneaky\n",
        })))
        .await
        .expect_err("edit refused for exec role");
        assert!(edit.to_string().contains("write"), "{edit}");
        // Execute is allowed (when a python3 interpreter is available).
        if python3_available() {
            let exec_result =
                (exec_only.execute)(make_ctx(json!({ "action": "execute", "path": path, "cell": 0 })))
                    .await
                    .expect("execute allowed for exec role");
            assert!(text_of(&exec_result).contains("[0] ok"), "{}", text_of(&exec_result));
        }
        let after = fs::read_to_string(&dir.path().join("notes.ipynb")).unwrap();
        assert_eq!(
            after,
            serde_json::to_string_pretty(&fixture_notebook()).unwrap(),
            "refused edit must leave the notebook untouched"
        );
    }

    #[test]
    fn source_to_lines_follows_nbformat_convention() {
        assert_eq!(
            source_to_lines("a = 1\nb = 2"),
            json!(["a = 1\n", "b = 2"])
        );
        assert_eq!(source_to_lines("a = 1\n"), json!(["a = 1\n"]));
        assert_eq!(source_to_lines(""), json!([""]));
    }
    // -----------------------------------------------------------------------
    // read bounds: cell cap, invalid JSON, metadata, cell types
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn notebook_read_caps_rendered_cells_at_two_hundred() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("big.ipynb");
        // 250 code cells: read must show the first 200 and report the
        // remainder omitted (the preview stays bounded for huge notebooks).
        // Source has no trailing newline so each cell renders as a single
        // preview line, keeping the total under the read output's 500-line
        // bounded() tail cap — otherwise the header itself would be dropped.
        let cells: Vec<Value> = (0..250)
            .map(|i| {
                json!({
                    "cell_type": "code",
                    "execution_count": null,
                    "metadata": {},
                    "outputs": [],
                    "source": [format!("x = {i}")]
                })
            })
            .collect();
        let doc = json!({
            "cells": cells,
            "metadata": {},
            "nbformat": 4,
            "nbformat_minor": 5,
        });
        fs::write(&path, serde_json::to_string(&doc).unwrap()).unwrap();
        let tool = notebook_tool(&dir.path().to_string_lossy());
        let text = text_of(&(tool.execute)(make_ctx(json!({ "action": "read", "path": path }))).await.expect("read"));
        assert!(text.contains("250 cells"), "{text}");
        assert!(text.contains("[199] code"), "{text}");
        assert!(text.contains("50 more cells omitted"), "{text}");
        // Cell 200 (0-based, the 201st) is beyond the cap and must not appear.
        assert!(!text.contains("[200] code"), "{text}");
    }

    #[tokio::test]
    async fn notebook_read_rejects_malformed_json() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("broken.ipynb");
        // Not valid JSON at all (a syntax error, distinct from the
        // "missing cells" case tested elsewhere).
        fs::write(&path, r#"{"cells": [broken"#).unwrap();
        let tool = notebook_tool(&dir.path().to_string_lossy());
        let error = (tool.execute)(make_ctx(json!({ "action": "read", "path": path })))
            .await
            .expect_err("malformed json must fail");
        assert!(error.to_string().contains("invalid JSON"), "{error}");
    }

    #[tokio::test]
    async fn notebook_read_truncates_large_metadata() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("meta.ipynb");
        // Metadata over 400 chars renders as "[truncated metadata]" so the
        // preview stays bounded.
        let big = "x".repeat(500);
        let doc = json!({
            "cells": [],
            "metadata": { "big_field": big },
            "nbformat": 4,
            "nbformat_minor": 5,
        });
        fs::write(&path, serde_json::to_string(&doc).unwrap()).unwrap();
        let tool = notebook_tool(&dir.path().to_string_lossy());
        let text = text_of(&(tool.execute)(make_ctx(json!({ "action": "read", "path": path }))).await.expect("read"));
        assert!(text.contains("[truncated metadata]"), "{text}");
    }

    #[tokio::test]
    async fn notebook_read_renders_raw_and_unknown_cell_types() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("mixed.ipynb");
        let doc = json!({
            "cells": [
                { "cell_type": "raw", "metadata": {}, "source": ["<raw>\n"] },
                { "cell_type": "custom-type", "metadata": {}, "source": ["??\n"] },
            ],
            "metadata": {},
            "nbformat": 4,
            "nbformat_minor": 5,
        });
        fs::write(&path, serde_json::to_string(&doc).unwrap()).unwrap();
        let tool = notebook_tool(&dir.path().to_string_lossy());
        let text = text_of(&(tool.execute)(make_ctx(json!({ "action": "read", "path": path }))).await.expect("read"));
        assert!(text.contains("[0] raw"), "{text}");
        assert!(text.contains("[1] custom-type"), "{text}");
    }

    #[tokio::test]
    async fn notebook_read_empty_notebook_reports_zero_cells() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("empty.ipynb");
        let doc = json!({ "cells": [], "metadata": {}, "nbformat": 4, "nbformat_minor": 5 });
        fs::write(&path, serde_json::to_string(&doc).unwrap()).unwrap();
        let tool = notebook_tool(&dir.path().to_string_lossy());
        let text = text_of(&(tool.execute)(make_ctx(json!({ "action": "read", "path": path }))).await.expect("read"));
        assert!(text.contains("0 cells"), "{text}");
    }

    // -----------------------------------------------------------------------
    // execute: timeout skip, empty cell, out-of-range cell
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn notebook_execute_times_out_and_skips_remaining_cells() {
        if !python3_available() {
            return;
        }
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("slow.ipynb");
        let doc = json!({
            "cells": [
                { "cell_type": "code", "execution_count": null, "metadata": {}, "outputs": [], "source": ["import time\ntime.sleep(30)\n"] },
                { "cell_type": "code", "execution_count": null, "metadata": {}, "outputs": [], "source": ["42\n"] },
            ],
            "metadata": {},
            "nbformat": 4,
            "nbformat_minor": 5,
        });
        fs::write(&path, serde_json::to_string(&doc).unwrap()).unwrap();
        let tool = notebook_tool(&dir.path().to_string_lossy());
        let result = (tool.execute)(make_ctx(json!({
            "action": "execute",
            "path": path,
            "timeout": 1,
        })))
        .await
        .expect("execute");
        let text = text_of(&result);
        // Cell 0 times out; cell 1 is skipped (never executed).
        assert!(text.contains("[0] timeout"), "{text}");
        assert!(text.contains("remaining cells skipped"), "{text}");
        assert!(!text.contains("result: 42"), "{text}");
        // No outputs were written back (write defaulted to false).
        let after = fs::read_to_string(&path).unwrap();
        assert!(after.contains("[]"), "outputs should not be written without write=true: {after}");
    }

    #[tokio::test]
    async fn notebook_execute_skips_empty_code_cell() {
        if !python3_available() {
            return;
        }
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("gaps.ipynb");
        let doc = json!({
            "cells": [
                { "cell_type": "code", "execution_count": null, "metadata": {}, "outputs": [], "source": ["   \n"] },
                { "cell_type": "code", "execution_count": null, "metadata": {}, "outputs": [], "source": ["7 * 6\n"] },
            ],
            "metadata": {},
            "nbformat": 4,
            "nbformat_minor": 5,
        });
        fs::write(&path, serde_json::to_string(&doc).unwrap()).unwrap();
        let tool = notebook_tool(&dir.path().to_string_lossy());
        let text = text_of(&(tool.execute)(make_ctx(json!({ "action": "execute", "path": path }))).await.expect("execute"));
        // The empty cell is skipped (not sent to the kernel); the second runs.
        assert!(text.contains("[0] code cell is empty — skipped"), "{text}");
        assert!(text.contains("[1] ok"), "{text}");
        assert!(text.contains("result: 42"), "{text}");
    }

    #[tokio::test]
    async fn notebook_execute_cell_index_out_of_range_is_rejected() {
        let dir = tempdir().expect("tempdir");
        let path = write_fixture(dir.path());
        let tool = notebook_tool(&dir.path().to_string_lossy());
        let error = (tool.execute)(make_ctx(json!({
            "action": "execute",
            "path": path,
            "cell": 99,
        })))
        .await
        .expect_err("out-of-range cell must fail");
        assert!(error.to_string().contains("out of range"), "{error}");
        assert!(error.to_string().contains("3 cells"), "{error}");
    }

    #[tokio::test]
    async fn notebook_execute_single_cell_runs_only_that_cell() {
        if !python3_available() {
            return;
        }
        let dir = tempdir().expect("tempdir");
        let path = write_fixture(dir.path());
        let tool = notebook_tool(&dir.path().to_string_lossy());
        let text = text_of(&(tool.execute)(make_ctx(json!({
            "action": "execute",
            "path": path,
            "cell": 0,
        }))).await.expect("execute single"));
        // Only cell 0 ran (a = 21); cell 2 (print + expression) did not.
        assert!(text.contains("[0] ok"), "{text}");
        assert!(!text.contains("[2]"), "{text}");
        assert!(!text.contains("result: 42"), "{text}");
    }

    // -----------------------------------------------------------------------
    // edit: validation
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn notebook_edit_rejects_bad_cell_type() {
        let dir = tempdir().expect("tempdir");
        let path = write_fixture(dir.path());
        let tool = notebook_tool(&dir.path().to_string_lossy());
        let error = (tool.execute)(make_ctx(json!({
            "action": "edit",
            "path": path,
            "cell_type": "delete",
            "source": "x",
        })))
        .await
        .expect_err("bad cell type must fail");
        assert!(error.to_string().contains("markdown, code, or raw"), "{error}");
        assert!(error.to_string().contains("delete"), "{error}");
    }

    #[tokio::test]
    async fn notebook_edit_rejects_empty_source() {
        let dir = tempdir().expect("tempdir");
        let path = write_fixture(dir.path());
        let tool = notebook_tool(&dir.path().to_string_lossy());
        let error = (tool.execute)(make_ctx(json!({
            "action": "edit",
            "path": path,
            "cell_type": "code",
            "source": "   ",
        })))
        .await
        .expect_err("empty source must fail");
        assert!(error.to_string().contains("non-empty"), "{error}");
    }

    #[tokio::test]
    async fn notebook_edit_appends_code_cell_from_array_source() {
        let dir = tempdir().expect("tempdir");
        let path = write_fixture(dir.path());
        let tool = notebook_tool(&dir.path().to_string_lossy());
        let result = (tool.execute)(make_ctx(json!({
            "action": "edit",
            "path": path,
            "cell_type": "code",
            "source": ["print(1)\n", "print(2)\n"],
        })))
        .await
        .expect("edit");
        assert!(text_of(&result).contains("appended code cell"), "{}", text_of(&result));
        let after: Value =
            serde_json::from_str(&fs::read_to_string(&dir.path().join("notes.ipynb")).unwrap()).unwrap();
        let cell = &after["cells"][3];
        assert_eq!(cell["cell_type"], "code");
        assert_eq!(cell["source"], json!(["print(1)\n", "print(2)\n"]));
        assert_eq!(cell["outputs"], json!([]));
        assert!(cell["execution_count"].is_null());
    }

    #[tokio::test]
    async fn notebook_edit_raw_cell_has_no_outputs_field() {
        let dir = tempdir().expect("tempdir");
        let path = write_fixture(dir.path());
        let tool = notebook_tool(&dir.path().to_string_lossy());
        (tool.execute)(make_ctx(json!({
            "action": "edit",
            "path": path,
            "cell_type": "raw",
            "source": "raw text\n",
        })))
        .await
        .expect("edit raw");
        let after: Value =
            serde_json::from_str(&fs::read_to_string(&dir.path().join("notes.ipynb")).unwrap()).unwrap();
        let cell = &after["cells"][3];
        assert_eq!(cell["cell_type"], "raw");
        assert_eq!(cell["source"], json!(["raw text\n"]));
        // Raw/markdown cells don't get outputs/execution_count (only code).
        assert!(cell.get("outputs").is_none() || cell["outputs"].is_null(), "{cell:?}");
    }

    // -----------------------------------------------------------------------
    // ipynb_outputs: full output shape coverage
    // -----------------------------------------------------------------------

    #[test]
    fn ipynb_outputs_classifies_syntax_error_as_syntaxerror() {
        let cell = CellResult {
            stdout: "out\n".into(),
            stderr: "err\n".into(),
            result: Some("42".into()),
            error: Some(CellError { kind: EvalErrorKind::Syntax, text: "SyntaxError: bad".into() }),
        };
        let outputs = ipynb_outputs(&cell);
        let arr = outputs.as_array().unwrap();
        // stdout stream, stderr stream, execute_result, error — in order.
        assert_eq!(arr.len(), 4);
        assert_eq!(arr[0]["output_type"], "stream");
        assert_eq!(arr[0]["name"], "stdout");
        assert_eq!(arr[1]["output_type"], "stream");
        assert_eq!(arr[1]["name"], "stderr");
        assert_eq!(arr[2]["output_type"], "execute_result");
        assert_eq!(arr[2]["data"]["text/plain"], "42");
        assert_eq!(arr[3]["output_type"], "error");
        assert_eq!(arr[3]["ename"], "SyntaxError");
    }

    #[test]
    fn ipynb_outputs_timeout_uses_timeouterror_ename() {
        let cell = CellResult {
            stdout: String::new(),
            stderr: String::new(),
            result: None,
            error: Some(CellError { kind: EvalErrorKind::Timeout, text: "timed out".into() }),
        };
        let outputs = ipynb_outputs(&cell);
        let arr = outputs.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["output_type"], "error");
        assert_eq!(arr[0]["ename"], "TimeoutError");
        assert_eq!(arr[0]["traceback"][0], "timed out");
    }

    #[test]
    fn ipynb_outputs_clean_cell_has_no_outputs() {
        let cell = CellResult {
            stdout: String::new(),
            stderr: String::new(),
            result: None,
            error: None,
        };
        let outputs = ipynb_outputs(&cell);
        assert!(outputs.as_array().unwrap().is_empty());
    }

    #[test]
    fn render_notebook_cell_includes_all_sections() {
        let cell = CellResult {
            stdout: "out line".into(),
            stderr: "err line".into(),
            result: Some("99".into()),
            error: Some(CellError { kind: EvalErrorKind::Runtime, text: "boom".into() }),
        };
        let text = render_notebook_cell(&cell);
        assert!(text.starts_with("runtime error"), "{text}");
        assert!(text.contains("stdout: out line"), "{text}");
        assert!(text.contains("stderr: err line"), "{text}");
        assert!(text.contains("result: 99"), "{text}");
        assert!(text.contains("error:"), "{text}");
        assert!(text.contains("boom"), "{text}");
    }
}
