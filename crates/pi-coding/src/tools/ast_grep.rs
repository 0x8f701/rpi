//! `ast_grep` tool: read-only structural code search powered by ast-grep
//! (tree-sitter). Matches ast-grep `$-metavariable` patterns against the AST,
//! which text grep cannot do (e.g. ignoring whitespace/comment drift, matching
//! by node kind).
//!
//! Sandbox: `path` is confined to the workspace roots via `resolve_scoped_path`
//! (same boundary as `grep`/`find`). Directory traversal reuses the parent
//! module's gitignore-aware `walk`. Only languages whose tree-sitter grammar
//! was compiled in (see `ENABLED_LANGS`) are searched; the rest are skipped
//! during directory walks and rejected with an actionable error when a single
//! file/target is given.

use std::path::Path;
use std::str::FromStr;

use anyhow::{anyhow, Result};
use serde_json::Value;

use ast_grep_core::{Language, Node, Pattern, PatternError};
use ast_grep_language::{LanguageExt, SupportLang};
use pi_agent::{AbortSignal, AgentTool, AgentToolResult, ToolCapability};

use crate::truncate::{format_size, truncate_head, DEFAULT_MAX_BYTES};

use super::{
    arg_str, check_aborted, factory_workspace, s_object, s_string, text_result, walk, IgnoreStack,
    WalkControl,
};
use super::paths::resolve_scoped_path;

/// Maximum matches reported before stopping (keeps agent context bounded).
const MATCH_LIMIT: usize = 50;

/// Languages compiled into this build. A curated subset of ast-grep's built-in
/// grammars; requesting anything else is rejected at the tool layer so the
/// disabled-language `unimplemented!()` stubs in `ast-grep-language` are never
/// reached. Keep this in sync with the feature flags in the workspace manifest.
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

/// Builds the `ast_grep` structural-search tool.
pub(crate) fn ast_grep_tool(cwd: &str) -> AgentTool {
    let workspace = factory_workspace(cwd);
    let params = s_object(
        vec![
            (
                "pattern",
                s_string(
                    "ast-grep pattern with $-metavariables, e.g. 'Some($A)', 'console.log($$$ARGS)', or 'fn $FNAME() {}'",
                ),
            ),
            (
                "path",
                s_string("File or directory to search (default: cwd). Confined to workspace roots."),
            ),
            (
                "lang",
                s_string(
                    "Language override (e.g. rust, typescript, tsx, javascript, python, go). \
Inferred from the path extension by default; unsupported languages are rejected.",
                ),
            ),
        ],
        vec!["pattern"],
    );
    let description = format!(
        "Search code structurally with ast-grep (tree-sitter) patterns. \
Reports {MATCH_LIMIT} matches as `path:line:col: first-line-of-match`. \
Respects .gitignore. Output is truncated to {}KB. \
Supports: {}.",
        DEFAULT_MAX_BYTES / 1024,
        enabled_lang_names()
    );
    AgentTool::new("ast_grep", description, params, move |ctx| {
        let workspace = workspace.clone();
        async move { run_ast_grep(&workspace, ctx.arguments, ctx.abort) }
    })
    .with_capability(ToolCapability::Read)
}

fn enabled_lang_names() -> String {
    ENABLED_LANGS
        .iter()
        .map(|l| l.to_string().to_lowercase())
        .collect::<Vec<_>>()
        .join(", ")
}

pub(crate) fn run_ast_grep(
    workspace: &crate::WorkspaceRoots,
    args: Value,
    abort: AbortSignal,
) -> Result<AgentToolResult> {
    check_aborted(&abort)?;
    let pattern = arg_str(&args, "pattern");
    if pattern.trim().is_empty() {
        return Err(anyhow!("pattern must not be empty"));
    }
    let root_path = match args
        .get("path")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        Some(p) => resolve_scoped_path(p, workspace)?,
        None => workspace.cwd().to_string_lossy().into_owned(),
    };
    let lang_override = args
        .get("lang")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());

    let info = std::fs::metadata(&root_path).map_err(|_| anyhow!("Path not found: {root_path}"))?;
    check_aborted(&abort)?;

    let mut lines: Vec<String> = Vec::new();
    let mut match_count = 0usize;
    let mut limit_reached = false;

    if !info.is_dir() {
        // Single-file (or single-target) mode: an unresolved language is a hard
        // error, not a silent skip.
        search_file(
            &root_path,
            &root_path,
            &pattern,
            lang_override,
            true,
            &mut lines,
            &mut match_count,
            &abort,
        )?;
    } else {
        let mut ig = IgnoreStack::new(&root_path, false, true);
        let mut hard_err: Option<anyhow::Error> = None;
        walk(&root_path, &mut ig, &mut |abs, rel, is_dir| {
            if abort.is_aborted() {
                return WalkControl::Stop;
            }
            if is_dir {
                return WalkControl::Continue;
            }
            if match_count >= MATCH_LIMIT {
                limit_reached = true;
                return WalkControl::Stop;
            }
            match search_file(
                abs,
                rel,
                &pattern,
                lang_override,
                false,
                &mut lines,
                &mut match_count,
                &abort,
            ) {
                Ok(()) => WalkControl::Continue,
                Err(e) => {
                    hard_err = Some(e);
                    WalkControl::Stop
                }
            }
        });
        if let Some(e) = hard_err {
            return Err(e);
        }
    }
    check_aborted(&abort)?;

    if match_count == 0 {
        return Ok(text_result("No matches found"));
    }
    let raw_output = lines.join("\n");
    let tr = truncate_head(&raw_output, usize::MAX, 0);
    let mut output = tr.content;
    let mut notices = Vec::new();
    if limit_reached {
        notices.push(format!(
            "{MATCH_LIMIT} matches limit reached. Refine the pattern or scope to a smaller path"
        ));
    }
    if tr.truncated {
        notices.push(format!("{} limit reached", format_size(DEFAULT_MAX_BYTES)));
    }
    if !notices.is_empty() {
        output.push_str("\n\n[");
        output.push_str(&notices.join(". "));
        output.push(']');
    }
    Ok(text_result(output))
}

/// Searches a single file. `require_lang` controls the unresolved-language
/// outcome: error (single-target mode) vs. silent skip (directory walk).
fn search_file(
    abs: &str,
    rel: &str,
    pattern: &str,
    lang_override: Option<&str>,
    require_lang: bool,
    lines: &mut Vec<String>,
    match_count: &mut usize,
    abort: &AbortSignal,
) -> Result<()> {
    if abort.is_aborted() {
        return Ok(());
    }
    let lang = match resolve_lang(abs, lang_override) {
        Some(l) => l,
        None => {
            if require_lang {
                return Err(anyhow!(
                    "unsupported language for {rel}; pass `lang` (one of: {})",
                    enabled_lang_names()
                ));
            }
            return Ok(());
        }
    };
    let pat = Pattern::try_new(pattern, lang).map_err(|e| pattern_error(e, pattern))?;
    let src = match std::fs::read_to_string(abs) {
        Ok(s) => s,
        Err(_) => return Ok(()), // unreadable/binary during a walk: skip
    };
    let root = lang.ast_grep(&src);
    let root_node = root.root();
    for matched in root_node.find_all(pat) {
        if abort.is_aborted() {
            return Ok(());
        }
        let pos = matched.start_pos();
        let line = pos.line() + 1;
        // `column` is O(line length); fine for reporting. 1-based char column.
        let col = pos.column(&*matched) + 1;
        let text = matched.text().to_string();
        let first_line = text.split('\n').next().unwrap_or("").trim_end();
        lines.push(format!("{rel}:{line}:{col}: {first_line}"));
        *match_count += 1;
        if *match_count >= MATCH_LIMIT {
            return Ok(());
        }
    }
    Ok(())
}

/// Resolves the ast-grep language for `path`/override. `None` means "skip or
/// unsupported": a `lang` override that parses but isn't compiled in, or an
/// extension that maps to a disabled language, both yield `None`.
fn resolve_lang(path: &str, lang_override: Option<&str>) -> Option<SupportLang> {
    if let Some(s) = lang_override {
        return SupportLang::from_str(s)
            .ok()
            .filter(|l| is_enabled(l));
    }
    SupportLang::from_path(Path::new(path)).filter(|l| is_enabled(l))
}

fn pattern_error(err: PatternError, pattern: &str) -> anyhow::Error {
    anyhow!("invalid ast-grep pattern `{pattern}`: {err}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    fn tmpdir() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("pi-astgrep-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn run(workspace: &crate::WorkspaceRoots, args: Value) -> Result<String> {
        let res = run_ast_grep(workspace, args, AbortSignal::none())?;
        Ok(res.content.first().and_then(|b| match b {
            pi_ai::ContentBlock::Text { text, .. } => Some(text.clone()),
            _ => None,
        }).unwrap_or_default())
    }

    #[test]
    fn pattern_matches_rust_some_capture() {
        let d = tmpdir();
        fs::write(
            d.join("lib.rs"),
            "fn main() {\n    let x = Some(123);\n    let y = Some(456);\n}\n",
        )
        .unwrap();
        let ws = crate::WorkspaceRoots::for_tool_factory(&d.to_string_lossy());
        let out = run(&ws, json!({ "pattern": "Some($A)", "path": "lib.rs" })).unwrap();
        assert!(out.contains("lib.rs:2:13: Some(123)"), "{out}");
        assert!(out.contains("lib.rs:3:13: Some(456)"), "{out}");
    }

    #[test]
    fn no_match_returns_no_matches() {
        let d = tmpdir();
        fs::write(d.join("lib.rs"), "fn main() { let x = 1; }\n").unwrap();
        let ws = crate::WorkspaceRoots::for_tool_factory(&d.to_string_lossy());
        let out = run(&ws, json!({ "pattern": "Some($A)", "path": "lib.rs" })).unwrap();
        assert_eq!(out, "No matches found");
    }

    #[test]
    fn directory_walk_searches_supported_langs_and_skips_unknown_extensions() {
        let d = tmpdir();
        fs::write(d.join("a.rs"), "fn f() { Some(1) }\n").unwrap();
        fs::write(d.join("b.py"), "x = Some(1)\n").unwrap(); // inferred as Python, searched too
        fs::write(d.join("c.txt"), "Some(1)\n").unwrap(); // unsupported extension, skipped
        let ws = crate::WorkspaceRoots::for_tool_factory(&d.to_string_lossy());
        let out = run(&ws, json!({ "pattern": "Some($A)", "path": "." })).unwrap();
        // Each supported file is parsed with its own grammar (per-file inference).
        assert!(out.contains("a.rs:1:10: Some(1)"), "{out}");
        assert!(out.contains("b.py:1:5: Some(1)"), "{out}");
        // Unsupported extension is skipped, not errored, during a walk.
        assert!(!out.contains("c.txt"), "{out}");
        assert!(!out.contains("unsupported language"), "{out}");
    }

    #[test]
    fn invalid_pattern_is_rejected_with_actionable_error() {
        let d = tmpdir();
        fs::write(d.join("lib.rs"), "fn main() {}\n").unwrap();
        let ws = crate::WorkspaceRoots::for_tool_factory(&d.to_string_lossy());
        let err = run(&ws, json!({ "pattern": "$$$A", "path": "lib.rs" }))
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid ast-grep pattern"), "{err}");
        assert!(err.contains("$$$A"), "{err}");
    }

    #[test]
    fn single_file_unsupported_language_errors() {
        let d = tmpdir();
        fs::write(d.join("data.unknownext"), "whatever").unwrap();
        let ws = crate::WorkspaceRoots::for_tool_factory(&d.to_string_lossy());
        let err = run(&ws, json!({ "pattern": "Some($A)", "path": "data.unknownext" }))
            .unwrap_err()
            .to_string();
        assert!(err.contains("unsupported language"), "{err}");
    }

    #[test]
    fn path_outside_workspace_roots_is_rejected() {
        let d = tmpdir();
        let outside = tmpdir();
        fs::write(outside.join("lib.rs"), "fn main() { Some(1) }\n").unwrap();
        let ws = crate::WorkspaceRoots::for_tool_factory(&d.to_string_lossy());
        let err = run(&ws, json!({ "pattern": "Some($A)", "path": outside.join("lib.rs") }))
            .unwrap_err()
            .to_string();
        assert!(err.contains("outside the workspace") || err.contains("escape"), "{err}");
    }

    #[test]
    fn enabled_langs_covers_curated_set() {
        assert!(is_enabled(&SupportLang::Rust));
        assert!(is_enabled(&SupportLang::TypeScript));
        assert!(is_enabled(&SupportLang::Tsx));
        assert!(is_enabled(&SupportLang::JavaScript));
        assert!(!is_enabled(&SupportLang::Ruby)); // not compiled in
    }

    #[test]
    fn resolve_lang_override_unsupported_returns_none() {
        assert_eq!(resolve_lang("x.rs", Some("rust")), Some(SupportLang::Rust));
        assert_eq!(resolve_lang("x.rs", Some("Ruby")), None); // valid lang name but disabled
        assert_eq!(resolve_lang("x.rs", Some("nonsense")), None);
        assert_eq!(resolve_lang("x.rs", None), Some(SupportLang::Rust));
        assert_eq!(resolve_lang("x.unknownext", None), None);
    }
}