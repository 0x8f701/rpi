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

use anyhow::{Result, anyhow};
use serde_json::Value;

use ast_grep_core::{Language, Node, Pattern, PatternError};
use ast_grep_language::{LanguageExt, SupportLang};
use pi_agent::{AbortSignal, AgentTool, AgentToolResult, ToolCapability};

use crate::truncate::{DEFAULT_MAX_BYTES, format_size, truncate_head};

use super::paths::resolve_scoped_path;
use super::{
    GenerationCaptureFn, arg_str, captured_policy_fs, check_aborted, factory_workspace, s_object,
    s_string, text_result,
};

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
                s_string(
                    "File or directory to search (default: cwd). Confined to workspace roots.",
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
    AgentTool::new("ast_grep", description, params, move |_ctx| async move {
        Err(anyhow!("ast_grep requires an application generation"))
    })
    .with_capability(ToolCapability::Read)
}

pub(crate) fn ast_grep_tool_with_generation(
    workspace: crate::WorkspaceRoots,
    capture: GenerationCaptureFn,
) -> AgentTool {
    let params = s_object(
        vec![
            ("pattern", s_string("ast-grep pattern")),
            ("path", s_string("Policy-authorized file to search")),
            ("lang", s_string("Optional language override")),
        ],
        vec!["pattern", "path"],
    );
    AgentTool::new(
        "ast_grep",
        "Search one file structurally through retained policy filesystem authority.",
        params,
        move |ctx| {
            let workspace = workspace.clone();
            let capture = capture.clone();
            async move {
                let generation = capture()?;
                run_ast_grep_captured(&workspace, &generation, ctx.arguments, ctx.abort)
            }
        },
    )
    .with_capability(ToolCapability::Read)
}

fn run_ast_grep_captured(
    workspace: &crate::WorkspaceRoots,
    generation: &std::sync::Arc<crate::application::generation::ApplicationGeneration>,
    args: Value,
    abort: AbortSignal,
) -> Result<AgentToolResult> {
    check_aborted(&abort)?;
    let pattern = arg_str(&args, "pattern");
    if pattern.trim().is_empty() {
        return Err(anyhow!("pattern must not be empty"));
    }
    let requested = arg_str(&args, "path");
    let absolute = std::path::PathBuf::from(resolve_scoped_path(&requested, workspace)?);
    let lang_override = args
        .get("lang")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty());
    let lang = resolve_lang(&requested, lang_override).ok_or_else(|| {
        anyhow!(
            "unsupported language for {requested}; pass `lang` (one of: {})",
            enabled_lang_names()
        )
    })?;
    let source = captured_policy_fs(generation)?.read_text_bounded(&absolute, 64 * 1024 * 1024)?;
    let pattern =
        Pattern::try_new(&pattern, lang).map_err(|error| pattern_error(error, &pattern))?;
    let root = lang.ast_grep(&source);
    let mut lines = Vec::new();
    for matched in root.root().find_all(pattern).take(MATCH_LIMIT) {
        check_aborted(&abort)?;
        let position = matched.start_pos();
        let first_line = matched
            .text()
            .split('\n')
            .next()
            .unwrap_or("")
            .trim_end()
            .to_owned();
        lines.push(format!(
            "{requested}:{}:{}: {first_line}",
            position.line() + 1,
            position.column(&*matched) + 1
        ));
    }
    if lines.is_empty() {
        Ok(text_result("No matches found"))
    } else {
        Ok(text_result(lines.join("\n")))
    }
}

fn enabled_lang_names() -> String {
    ENABLED_LANGS
        .iter()
        .map(|l| l.to_string().to_lowercase())
        .collect::<Vec<_>>()
        .join(", ")
}

pub(crate) fn run_ast_grep(
    _workspace: &crate::WorkspaceRoots,
    _args: Value,
    _abort: AbortSignal,
) -> Result<AgentToolResult> {
    Err(anyhow!("ast_grep requires an application generation"))
}

/// Resolves the ast-grep language for `path`/override. `None` means "skip or
/// unsupported": a `lang` override that parses but isn't compiled in, or an
/// extension that maps to a disabled language, both yield `None`.
fn resolve_lang(path: &str, lang_override: Option<&str>) -> Option<SupportLang> {
    if let Some(s) = lang_override {
        return SupportLang::from_str(s).ok().filter(|l| is_enabled(l));
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
    use std::path::Path;

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
        let api = format!("ast-grep-{suffix}");
        let provider = format!("ast-grep-{suffix}");
        let model = pi_ai::Model {
            id: format!("ast-grep-{suffix}"),
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
        .expect("publish ast grep application");
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

    fn tmpdir() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("pi-astgrep-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn text_of(result: AgentToolResult) -> String {
        result
            .content
            .first()
            .and_then(|block| match block {
                pi_ai::ContentBlock::Text { text, .. } => Some(text.clone()),
                _ => None,
            })
            .unwrap_or_default()
    }

    fn run(fixture: &GenerationFixture, args: Value) -> Result<String> {
        let generation = (fixture.capture)()?;
        run_ast_grep_captured(&fixture.workspace, &generation, args, AbortSignal::none())
            .map(text_of)
    }

    #[tokio::test]
    async fn pattern_matches_rust_some_capture() {
        let d = tmpdir();
        fs::write(
            d.join("lib.rs"),
            "fn main() {\n    let x = Some(123);\n    let y = Some(456);\n}\n",
        )
        .unwrap();
        let fixture = generation_fixture(&d).await;
        let out = run(&fixture, json!({ "pattern": "Some($A)", "path": "lib.rs" })).unwrap();
        assert!(out.contains("lib.rs:2:13: Some(123)"), "{out}");
        assert!(out.contains("lib.rs:3:13: Some(456)"), "{out}");
    }

    #[tokio::test]
    async fn no_match_returns_no_matches() {
        let d = tmpdir();
        fs::write(d.join("lib.rs"), "fn main() { let x = 1; }\n").unwrap();
        let fixture = generation_fixture(&d).await;
        let out = run(&fixture, json!({ "pattern": "Some($A)", "path": "lib.rs" })).unwrap();
        assert_eq!(out, "No matches found");
    }

    #[tokio::test]
    async fn directory_walk_searches_supported_langs_and_skips_unknown_extensions() {
        let d = tmpdir();
        fs::write(d.join("a.rs"), "fn f() { Some(1) }\n").unwrap();
        fs::write(d.join("b.py"), "x = Some(1)\n").unwrap();
        fs::write(d.join("c.txt"), "Some(1)\n").unwrap();
        let fixture = generation_fixture(&d).await;
        let rust = run(&fixture, json!({ "pattern": "Some($A)", "path": "a.rs" })).unwrap();
        let python = run(&fixture, json!({ "pattern": "Some($A)", "path": "b.py" })).unwrap();
        assert!(rust.contains("a.rs:1:10: Some(1)"), "{rust}");
        assert!(python.contains("b.py:1:5: Some(1)"), "{python}");
        let unsupported = run(&fixture, json!({ "pattern": "Some($A)", "path": "c.txt" }))
            .expect_err("unsupported extension remains outside the searched language set")
            .to_string();
        assert!(
            unsupported.contains("unsupported language"),
            "{unsupported}"
        );
    }

    #[tokio::test]
    async fn invalid_pattern_is_rejected_with_actionable_error() {
        let d = tmpdir();
        fs::write(d.join("lib.rs"), "fn main() {}\n").unwrap();
        let fixture = generation_fixture(&d).await;
        let err = run(&fixture, json!({ "pattern": "$$$A", "path": "lib.rs" }))
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid ast-grep pattern"), "{err}");
        assert!(err.contains("$$$A"), "{err}");
    }

    #[tokio::test]
    async fn single_file_unsupported_language_errors() {
        let d = tmpdir();
        fs::write(d.join("data.unknownext"), "whatever").unwrap();
        let fixture = generation_fixture(&d).await;
        let err = run(
            &fixture,
            json!({ "pattern": "Some($A)", "path": "data.unknownext" }),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("unsupported language"), "{err}");
    }

    #[tokio::test]
    async fn path_outside_workspace_roots_is_rejected() {
        let d = tmpdir();
        let outside = tmpdir();
        fs::write(outside.join("lib.rs"), "fn main() { Some(1) }\n").unwrap();
        let fixture = generation_fixture(&d).await;
        let err = run(
            &fixture,
            json!({ "pattern": "Some($A)", "path": outside.join("lib.rs") }),
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.starts_with("path_outside_workspace:")
                && err.contains("outside authorized workspace roots"),
            "{err}"
        );
    }

    #[test]
    fn enabled_langs_covers_curated_set() {
        assert!(is_enabled(&SupportLang::Rust));
        assert!(is_enabled(&SupportLang::TypeScript));
        assert!(is_enabled(&SupportLang::Tsx));
        assert!(is_enabled(&SupportLang::JavaScript));
        assert!(!is_enabled(&SupportLang::Ruby));
    }

    #[test]
    fn resolve_lang_override_unsupported_returns_none() {
        assert_eq!(resolve_lang("x.rs", Some("rust")), Some(SupportLang::Rust));
        assert_eq!(resolve_lang("x.rs", Some("Ruby")), None);
        assert_eq!(resolve_lang("x.rs", Some("nonsense")), None);
        assert_eq!(resolve_lang("x.rs", None), Some(SupportLang::Rust));
        assert_eq!(resolve_lang("x.unknownext", None), None);
    }
}
