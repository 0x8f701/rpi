//! `ast_edit` tool: structural rewrite powered by ast-grep (tree-sitter).
//!
//! Runs a single ast-grep pattern→rewrite replacement over one file, applies
//! every non-overlapping match, and writes the result back under the per-file
//! mutation queue (the same lock `edit`/`write` use) so concurrent mutations to
//! the same file serialize.
//!
//! The pattern is validated via `Pattern::try_new` so malformed patterns are
//! rejected with an actionable message before any file is touched. The rewrite
//! is an ast-grep template (metavariables substituted per match); it has no
//! separate parse step.

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

use ast_grep_core::{Language, Pattern, PatternError};
use ast_grep_language::{LanguageExt, SupportLang};
use pi_agent::{AbortSignal, AgentTool, AgentToolResult, ToolCapability};

use super::{
    arg_str, check_aborted, ensure_regular_mutation_target, factory_workspace, s_object, s_string,
    text_result,
};
use super::editdiff::generate_edit_details;
use super::mutation_queue::with_file_mutation_queue;
use super::paths::resolve_mutation_path;

use std::path::Path;
use std::str::FromStr;

/// Bound for the inline before→after diff in the tool result.
const DIFF_MAX_BYTES: usize = 2 * 1024;
const ENABLED_LANGS: &[SupportLang] = &[
    SupportLang::Rust,
    SupportLang::TypeScript,
    SupportLang::Tsx,
    SupportLang::JavaScript,
    SupportLang::Python,
    SupportLang::Go,
    SupportLang::C,
    SupportLang::Cpp,
    SupportLang::Java,
    SupportLang::Json,
    SupportLang::Html,
    SupportLang::Css,
    SupportLang::Bash,
    SupportLang::Markdown,
    SupportLang::Yaml,
];

fn is_enabled(lang: &SupportLang) -> bool {
    ENABLED_LANGS.contains(lang)
}

fn enabled_lang_names() -> String {
    ENABLED_LANGS
        .iter()
        .map(|l| l.to_string().to_lowercase())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Builds the `ast_edit` structural-rewrite tool.
pub(crate) fn ast_edit_tool(cwd: &str) -> AgentTool {
    let workspace = factory_workspace(cwd);
    let params = s_object(
        vec![
            (
                "pattern",
                s_string("ast-grep pattern to match, with $-metavariables, e.g. 'Some($A)'"),
            ),
            (
                "rewrite",
                s_string(
                    "Replacement template; $-metavariables captured by the pattern are substituted per match.",
                ),
            ),
            (
                "path",
                s_string(
                    "Single file to rewrite. Relative paths resolve from cwd; absolute and parent-relative paths may target ordinary filesystem locations outside workspace roots.",
                ),
            ),
            (
                "lang",
                s_string(
                    "Language override (e.g. rust, typescript, tsx, javascript, python, go). \
Inferred from the path extension by default; unsupported languages are rejected.",
                ),
            ),
        ],
        vec!["pattern", "rewrite", "path"],
    );
    let description = format!(
        "Rewrite a single file structurally with ast-grep (tree-sitter): every non-overlapping \
match of `pattern` is replaced by `rewrite` (metavariables substituted). Reports the replacement \
count and a bounded before→after diff. Supports: {}.",
        enabled_lang_names()
    );
    AgentTool::new("ast_edit", description, params, move |ctx| {
        let workspace = workspace.clone();
        async move { run_ast_edit(&workspace, ctx.arguments, ctx.abort).await }
    })
    .with_capability(ToolCapability::Write)
}

pub(crate) async fn run_ast_edit(
    workspace: &crate::WorkspaceRoots,
    args: Value,
    abort: AbortSignal,
) -> Result<AgentToolResult> {
    let path = arg_str(&args, "path");
    let pattern = arg_str(&args, "pattern");
    let rewrite = arg_str(&args, "rewrite");
    if pattern.trim().is_empty() {
        return Err(anyhow!("pattern must not be empty"));
    }
    let lang_override = args
        .get("lang")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());

    let abs = resolve_mutation_path(&path, workspace)?;
    // Serialize with edit/write to the same file.
    with_file_mutation_queue(&abs, || async {
        check_aborted(&abort)?;
        ensure_regular_mutation_target(&abs, &path, "ast_edit", false)?;
        let old_content = std::fs::read_to_string(&abs)
            .map_err(|e| anyhow!("Could not read {path}: {}", super::fs_error_code(&e)))?;
        check_aborted(&abort)?;

        let lang = resolve_lang(&abs, lang_override).ok_or_else(|| {
            anyhow!(
                "unsupported language for {path}; pass `lang` (one of: {})",
                enabled_lang_names()
            )
        })?;
        let pat = Pattern::try_new(&pattern, lang).map_err(|e| pattern_error(e, &pattern))?;

        // Parse and compute all non-overlapping replacement edits up front, so
        // a rewrite that reintroduces the pattern cannot loop forever.
        let root = lang.ast_grep(&old_content);
        let edits = root.root().replace_all(pat, rewrite.as_str());
        let n = edits.len();
        if n == 0 {
            return Ok(text_result(format!("No replacements in {path}.")));
        }
        let new_content = apply_edits(&old_content, &edits);
        check_aborted(&abort)?;
        std::fs::write(&abs, new_content.as_bytes())
            .map_err(|e| anyhow!("Could not write {path}: {}", super::fs_error_code(&e)))?;
        check_aborted(&abort)?;

        let details = generate_edit_details(&path, &old_content, &new_content);
        let diff_preview = truncate_string(&details.diff, DIFF_MAX_BYTES);
        let mut output = format!("{n} replacement(s) in {path}.\n\n{diff_preview}");
        if details.diff.len() > DIFF_MAX_BYTES {
            output.push_str("\n\n[diff truncated]");
        }
        let mut res = text_result(output);
        res.details = json!({
            "diff": details.diff,
            "patch": details.patch,
            "firstChangedLine": details.first_changed_line,
            "replacements": n,
        });
        Ok(res)
    })
    .await
}

/// Resolves the ast-grep language for `path`/override, restricted to the
/// compiled-in grammar subset.
fn resolve_lang(path: &str, lang_override: Option<&str>) -> Option<SupportLang> {
    if let Some(s) = lang_override {
        return SupportLang::from_str(s).ok().filter(|l| is_enabled(l));
    }
    SupportLang::from_path(Path::new(path)).filter(|l| is_enabled(l))
}

fn pattern_error(err: PatternError, pattern: &str) -> anyhow::Error {
    anyhow!("invalid ast-grep pattern `{pattern}`: {err}")
}

/// Applies ast-grep `Edit<String>` edits (byte offsets, non-overlapping) to the
/// original source. Edits are applied in descending position order so earlier
/// offsets remain valid. `Edit::inserted_text` is `Vec<u8>` for a `String`
/// source; splicing valid UTF-8 at tree-sitter node boundaries yields valid
/// UTF-8, so `from_utf8_lossy` is a safe fallback.
fn apply_edits(src: &str, edits: &[ast_grep_core::source::Edit<String>]) -> String {
    if edits.is_empty() {
        return src.to_string();
    }
    let mut bytes: Vec<u8> = src.as_bytes().to_vec();
    let mut ordered: Vec<&ast_grep_core::source::Edit<String>> = edits.iter().collect();
    ordered.sort_by(|a, b| b.position.cmp(&a.position));
    for e in ordered {
        let pos = e.position.min(bytes.len());
        let end = (pos + e.deleted_length).min(bytes.len());
        bytes.splice(pos..end, e.inserted_text.iter().copied());
    }
    String::from_utf8(bytes).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

fn truncate_string(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut cut = max;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut out = s[..cut].to_string();
    out.push_str("…");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    fn tmpdir() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("pi-astedit-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn text_of(res: &pi_agent::AgentToolResult) -> String {
        res.content
            .first()
            .and_then(|b| match b {
                pi_ai::ContentBlock::Text { text, .. } => Some(text.clone()),
                _ => None,
            })
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn replaces_all_non_overlapping_matches() {
        let d = tmpdir();
        fs::write(d.join("lib.rs"), "let a = 1;\nlet b = 1;\n").unwrap();
        let ws = crate::WorkspaceRoots::for_tool_factory(&d.to_string_lossy());
        let res = run_ast_edit(
            &ws,
            json!({ "pattern": "let $A = 1;", "rewrite": "let $A = 2;", "path": "lib.rs" }),
            AbortSignal::none(),
        )
        .await
        .unwrap();
        let text = text_of(&res);
        assert!(text.contains("2 replacement(s) in lib.rs."), "{text}");
        let after = fs::read_to_string(d.join("lib.rs")).unwrap();
        assert_eq!(after, "let a = 2;\nlet b = 2;\n");
    }

    #[tokio::test]
    async fn substitutes_metavariables_in_rewrite() {
        let d = tmpdir();
        fs::write(d.join("lib.rs"), "fn main() { Some(123) }\n").unwrap();
        let ws = crate::WorkspaceRoots::for_tool_factory(&d.to_string_lossy());
        let res = run_ast_edit(
            &ws,
            json!({ "pattern": "Some($A)", "rewrite": "Ok($A)", "path": "lib.rs" }),
            AbortSignal::none(),
        )
        .await
        .unwrap();
        assert!(text_of(&res).contains("1 replacement(s) in lib.rs."));
        assert_eq!(fs::read_to_string(d.join("lib.rs")).unwrap(), "fn main() { Ok(123) }\n");
    }

    #[tokio::test]
    async fn no_match_reports_zero() {
        let d = tmpdir();
        fs::write(d.join("lib.rs"), "let x = 5;\n").unwrap();
        let ws = crate::WorkspaceRoots::for_tool_factory(&d.to_string_lossy());
        let res = run_ast_edit(
            &ws,
            json!({ "pattern": "Some($A)", "rewrite": "Ok($A)", "path": "lib.rs" }),
            AbortSignal::none(),
        )
        .await
        .unwrap();
        assert!(text_of(&res).contains("No replacements in lib.rs."), "{}", text_of(&res));
        assert_eq!(fs::read_to_string(d.join("lib.rs")).unwrap(), "let x = 5;\n");
    }

    #[tokio::test]
    async fn invalid_pattern_is_rejected_before_write() {
        let d = tmpdir();
        let original = "let x = 1;\n";
        fs::write(d.join("lib.rs"), original).unwrap();
        let ws = crate::WorkspaceRoots::for_tool_factory(&d.to_string_lossy());
        let err = run_ast_edit(
            &ws,
            json!({ "pattern": "$$$A", "rewrite": "let x = 2;", "path": "lib.rs" }),
            AbortSignal::none(),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("invalid ast-grep pattern"), "{err}");
        // File untouched: pattern validation precedes the write.
        assert_eq!(fs::read_to_string(d.join("lib.rs")).unwrap(), original);
    }

    #[tokio::test]
    async fn missing_file_errors() {
        let d = tmpdir();
        let ws = crate::WorkspaceRoots::for_tool_factory(&d.to_string_lossy());
        let err = run_ast_edit(
            &ws,
            json!({ "pattern": "let $A = 1;", "rewrite": "let $A = 2;", "path": "nope.rs" }),
            AbortSignal::none(),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("nope.rs"), "{err}");
        // fs_error_code renders "ENOENT" for a missing file.
        assert!(err.contains("ENOENT") || err.contains("not"), "{err}");
    }

    #[tokio::test]
    async fn unsupported_language_errors() {
        let d = tmpdir();
        fs::write(d.join("data.unknownext"), "whatever").unwrap();
        let ws = crate::WorkspaceRoots::for_tool_factory(&d.to_string_lossy());
        let err = run_ast_edit(
            &ws,
            json!({ "pattern": "Some($A)", "rewrite": "Ok($A)", "path": "data.unknownext" }),
            AbortSignal::none(),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("unsupported language"), "{err}");
    }

    #[tokio::test]
    async fn mutation_queue_serializes_concurrent_edits() {
        let d = tmpdir();
        fs::write(d.join("lib.rs"), "let a = 1;\nlet b = 2;\n").unwrap();
        let ws = crate::WorkspaceRoots::for_tool_factory(&d.to_string_lossy());
        let ws1 = ws.clone();
        let ws2 = ws.clone();
        // Two commutative structural edits hit the same file concurrently.
        let (r1, r2) = tokio::join!(
            async move {
                run_ast_edit(
                    &ws1,
                    json!({ "pattern": "let a = $X", "rewrite": "let a = $X + 0", "path": "lib.rs" }),
                    AbortSignal::none(),
                )
                .await
            },
            async move {
                run_ast_edit(
                    &ws2,
                    json!({ "pattern": "let b = $X", "rewrite": "let b = $X + 0", "path": "lib.rs" }),
                    AbortSignal::none(),
                )
                .await
            },
        );
        r1.unwrap();
        r2.unwrap();
        let final_content = fs::read_to_string(d.join("lib.rs")).unwrap();
        // Both edits landed (no lost update) — serialization, not racing.
        assert!(final_content.contains("let a = 1 + 0;"), "{final_content}");
        assert!(final_content.contains("let b = 2 + 0;"), "{final_content}");
    }
}