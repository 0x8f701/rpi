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

use anyhow::{Result, anyhow};
use serde_json::{Value, json};

use ast_grep_core::{Language, Pattern, PatternError};
use ast_grep_language::{LanguageExt, SupportLang};
use pi_agent::{AbortSignal, AgentTool, AgentToolResult, ToolCapability};

use super::captured_policy_fs;
use super::editdiff::generate_edit_details;
use super::mutation_queue::with_policy_mutation_queue;
use super::paths::resolve_mutation_path;
use super::{
    GenerationCaptureFn, arg_str, check_aborted, factory_workspace, s_object, s_string, text_result,
};

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
    AgentTool::new("ast_edit", description, params, move |_ctx| async move {
        Err(anyhow!("ast_edit requires an application generation"))
    })
    .with_capability(ToolCapability::Write)
}

pub(crate) fn ast_edit_tool_with_generation(
    workspace: crate::WorkspaceRoots,
    capture: GenerationCaptureFn,
) -> AgentTool {
    let params = s_object(
        vec![
            ("pattern", s_string("ast-grep pattern to match")),
            ("rewrite", s_string("Replacement template")),
            ("path", s_string("Policy-authorized file to rewrite")),
            ("lang", s_string("Optional language override")),
        ],
        vec!["pattern", "rewrite", "path"],
    );
    AgentTool::new(
        "ast_edit",
        "Rewrite a file structurally through retained policy filesystem authority.",
        params,
        move |ctx| {
            let workspace = workspace.clone();
            let capture = capture.clone();
            async move {
                let generation = capture()?;
                run_ast_edit_captured(&workspace, &generation, ctx.arguments, ctx.abort).await
            }
        },
    )
    .with_capability(ToolCapability::Write)
}

async fn run_ast_edit_captured(
    workspace: &crate::WorkspaceRoots,
    generation: &std::sync::Arc<crate::application::generation::ApplicationGeneration>,
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
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty());
    let absolute = std::path::PathBuf::from(resolve_mutation_path(&path, workspace)?);
    let fs = captured_policy_fs(generation)?;
    let opened = fs.open_file(&absolute)?;
    let expected = opened.identity();
    let (root_identity, relative) = fs.mutation_coordinate(&absolute)?;
    with_policy_mutation_queue(fs.revision(), root_identity, &relative, || async {
        check_aborted(&abort)?;
        let old_content = String::from_utf8(opened.read_bounded(64 * 1024 * 1024)?)
            .map_err(|_| anyhow!("Could not read {path}: invalid UTF-8"))?;
        let lang = resolve_lang(&path, lang_override).ok_or_else(|| {
            anyhow!(
                "unsupported language for {path}; pass `lang` (one of: {})",
                enabled_lang_names()
            )
        })?;
        let pat =
            Pattern::try_new(&pattern, lang).map_err(|error| pattern_error(error, &pattern))?;
        let root = lang.ast_grep(&old_content);
        let edits = root.root().replace_all(pat, rewrite.as_str());
        if edits.is_empty() {
            return Ok(text_result(format!("No replacements in {path}.")));
        }
        let count = edits.len();
        let new_content = apply_edits(&old_content, &edits);
        fs.replace_file(crate::fs::PolicyReplacement {
            path: absolute.clone(),
            expected: Some(expected),
            bytes: new_content.as_bytes().to_vec(),
            mode: crate::fs::FileMode::Shared,
        })?;
        check_aborted(&abort)?;
        let details = generate_edit_details(&path, &old_content, &new_content);
        let diff_preview = truncate_string(&details.diff, DIFF_MAX_BYTES);
        let mut output = format!("{count} replacement(s) in {path}.\n\n{diff_preview}");
        if details.diff.len() > DIFF_MAX_BYTES {
            output.push_str("\n\n[diff truncated]");
        }
        let mut result = text_result(output);
        result.details = json!({
            "diff": details.diff,
            "patch": details.patch,
            "firstChangedLine": details.first_changed_line,
            "replacements": count,
        });
        Ok(result)
    })
    .await
}

pub(crate) async fn run_ast_edit(
    _workspace: &crate::WorkspaceRoots,
    _args: Value,
    _abort: AbortSignal,
) -> Result<AgentToolResult> {
    Err(anyhow!("ast_edit requires an application generation"))
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
    use std::path::Path;
    use std::sync::Arc;

    struct GenerationFixture {
        _agent: tempfile::TempDir,
        _sessions: tempfile::TempDir,
        _resources: crate::ResourceManager,
        _session: crate::Session,
        _application: crate::Application,
        _faux: pi_ai::providers::FauxProviderRegistration,
        workspace: crate::WorkspaceRoots,
        capture: GenerationCaptureFn,
    }

    async fn generation_fixture(cwd: &Path) -> GenerationFixture {
        let agent = tempfile::tempdir().expect("agent dir");
        let sessions = tempfile::tempdir().expect("session dir");
        fs::write(
            agent.path().join("settings.json"),
            r#"{"sandbox":{"mode":"danger-full-access"}}"#,
        )
        .expect("sandbox settings");
        let mut resource_options = crate::ResourceManagerOptions::new(cwd);
        resource_options.agent_dir = agent.path().to_path_buf();
        resource_options.project_trust_override = Some(true);
        resource_options.disable_extensions = true;
        resource_options.disable_skills = true;
        resource_options.disable_prompt_templates = true;
        resource_options.disable_themes = true;
        resource_options.disable_context_files = true;
        let resources = crate::ResourceManager::new(resource_options).expect("resources");
        let suffix = uuid::Uuid::new_v4().to_string();
        let api = format!("ast-edit-{suffix}");
        let provider = format!("ast-edit-{suffix}");
        let model = pi_ai::Model {
            id: format!("ast-edit-{suffix}"),
            api: api.clone(),
            provider: provider.clone(),
            ..pi_ai::Model::default()
        };
        let faux =
            pi_ai::providers::register_faux_provider(pi_ai::providers::FauxProviderOptions {
                api,
                provider,
                models: vec![model.clone()],
                chunk_size: 1,
            });
        let workspace =
            crate::WorkspaceRoots::new(cwd, Vec::<std::path::PathBuf>::new()).expect("workspace");
        let session =
            crate::Session::new_with_additional_tools_filtered_discovery_workspace_and_uri(
                crate::SessionOptions {
                    model,
                    cwd: cwd.to_path_buf(),
                    system_prompt: String::new(),
                    thinking_level: pi_agent::ThinkingLevel::Off,
                    api_key: String::new(),
                    compaction: None,
                    stream_options: Default::default(),
                    tools: Some(Vec::new()),
                    before_tool_call: None,
                    after_tool_call: None,
                    stream_fn: None,
                    auth_resolver: None,
                    auth_manager: None,
                },
                Vec::new(),
                crate::ToolSelection::default(),
                crate::ResourceDiscovery::Disabled,
                workspace.clone(),
                None,
            )
            .expect("session");
        session.set_session_dir(sessions.path().to_path_buf());
        session
            .attach_resources(resources.clone())
            .await
            .expect("attach resources");
        let application = crate::Application::from_runtime_candidate(
            crate::ApplicationRuntimeCandidate::new(session.clone()),
        )
        .await
        .expect("publish ast edit application");
        let capture = application.generation_capture_fn();
        GenerationFixture {
            _agent: agent,
            _sessions: sessions,
            _resources: resources,
            _session: session,
            _application: application,
            _faux: faux,
            workspace,
            capture,
        }
    }

    async fn run(fixture: &GenerationFixture, args: Value) -> Result<AgentToolResult> {
        let generation = (fixture.capture)()?;
        run_ast_edit_captured(&fixture.workspace, &generation, args, AbortSignal::none()).await
    }

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
        let fixture = generation_fixture(&d).await;
        let res = run(
            &fixture,
            json!({ "pattern": "let $A = 1;", "rewrite": "let $A = 2;", "path": "lib.rs" }),
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
        let fixture = generation_fixture(&d).await;
        let res = run(
            &fixture,
            json!({ "pattern": "Some($A)", "rewrite": "Ok($A)", "path": "lib.rs" }),
        )
        .await
        .unwrap();
        assert!(text_of(&res).contains("1 replacement(s) in lib.rs."));
        assert_eq!(
            fs::read_to_string(d.join("lib.rs")).unwrap(),
            "fn main() { Ok(123) }\n"
        );
    }

    #[tokio::test]
    async fn no_match_reports_zero() {
        let d = tmpdir();
        fs::write(d.join("lib.rs"), "let x = 5;\n").unwrap();
        let fixture = generation_fixture(&d).await;
        let res = run(
            &fixture,
            json!({ "pattern": "Some($A)", "rewrite": "Ok($A)", "path": "lib.rs" }),
        )
        .await
        .unwrap();
        assert!(
            text_of(&res).contains("No replacements in lib.rs."),
            "{}",
            text_of(&res)
        );
        assert_eq!(
            fs::read_to_string(d.join("lib.rs")).unwrap(),
            "let x = 5;\n"
        );
    }

    #[tokio::test]
    async fn invalid_pattern_is_rejected_before_write() {
        let d = tmpdir();
        let original = "let x = 1;\n";
        fs::write(d.join("lib.rs"), original).unwrap();
        let fixture = generation_fixture(&d).await;
        let err = run(
            &fixture,
            json!({ "pattern": "$$$A", "rewrite": "let x = 2;", "path": "lib.rs" }),
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
        let fixture = generation_fixture(&d).await;
        let err = run(
            &fixture,
            json!({ "pattern": "let $A = 1;", "rewrite": "let $A = 2;", "path": "nope.rs" }),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("filesystem operation OpenEntry failed") && err.contains("NotFound"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn unsupported_language_errors() {
        let d = tmpdir();
        fs::write(d.join("data.unknownext"), "whatever").unwrap();
        let fixture = generation_fixture(&d).await;
        let err = run(
            &fixture,
            json!({ "pattern": "Some($A)", "rewrite": "Ok($A)", "path": "data.unknownext" }),
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
        let fixture = generation_fixture(&d).await;
        let generation = (fixture.capture)().expect("published generation");
        let ws1 = fixture.workspace.clone();
        let ws2 = fixture.workspace.clone();
        let generation1 = generation.clone();
        let generation2 = generation.clone();
        // Two commutative structural edits hit the same generation-bound file concurrently.
        let (r1, r2) = tokio::join!(
            async move {
                run_ast_edit_captured(
                    &ws1,
                    &generation1,
                    json!({ "pattern": "let a = $X", "rewrite": "let a = $X + 0", "path": "lib.rs" }),
                    AbortSignal::none(),
                )
                .await
            },
            async move {
                run_ast_edit_captured(
                    &ws2,
                    &generation2,
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
