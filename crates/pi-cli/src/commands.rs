//! Subcommand handlers: `models`, `sessions`, `import-session`, and `export`.
//!
//! These do not depend on the live Session facade — they read the static
//! model catalog, the session store, or the import/export adapters
//! respectively.
use std::path::Path;

use anyhow::{Context, Result};

use pi_ai::{get_models, get_providers};

use crate::output::{BOLD, RESET, warn_line};

/// List available models, optionally filtered by a substring matched against
/// the provider name or model id (case-sensitive, matching the Go CLI).
///
/// Provider headers are printed bold. When nothing matches the filter (or the
/// catalog is empty), stdout stays silent — no extra empty-result prose.
pub async fn list_models(filter: Option<&str>, application: &pi_coding::Application) -> Result<()> {
    let manager = pi_coding::LlamaManager::global()?;
    if manager.effective_settings()?.is_some() && !crate::session_run::offline() {
        let refreshed = manager
            .refresh_catalog(application.http_executor(pi_coding::HostHttpClass::LlamaRouter))
            .await?;
        if let Some(warning) = refreshed.warning {
            warn_line(&format!(
                "Warning: llama.cpp router unavailable; using cached catalog: {warning}"
            ));
        }
    } else if pi_ai::get_models(pi_ai::LLAMA_PROVIDER_ID).is_empty() {
        manager.load_cached_catalog()?;
    }
    let mut providers = get_providers();
    providers.sort();
    let mut models = providers
        .into_iter()
        .flat_map(|provider| get_models(&provider))
        .collect::<Vec<_>>();
    models = crate::models_config::filter_models_for_resolved_auth_async(models, None).await;
    let mut grouped = std::collections::BTreeMap::<String, Vec<String>>::new();
    for model in models {
        if filter.is_none_or(|needle| model.id.contains(needle) || model.provider.contains(needle))
        {
            grouped.entry(model.provider).or_default().push(model.id);
        }
    }
    for (provider, mut ids) in grouped {
        ids.sort();
        println!("{BOLD}{provider}{RESET}");
        for id in ids {
            println!("  {id}");
        }
    }
    Ok(())
}

/// Refresh configured dynamic model catalogs.
pub async fn refresh_model_catalogs(application: &pi_coding::Application) -> Result<()> {
    crate::models_config::load_custom_models()?;
    if let Some(warning) = crate::models_config::refresh_radius_catalog().await? {
        warn_line(&format!(
            "Warning: Radius gateway unavailable; using cached catalog: {warning}"
        ));
    }
    let manager = pi_coding::LlamaManager::global()?;
    if manager.effective_settings()?.is_some() {
        manager
            .refresh_catalog(application.http_executor(pi_coding::HostHttpClass::LlamaRouter))
            .await?;
    }
    println!("Model catalogs refreshed");
    Ok(())
}

/// List saved sessions for `cwd` (newest first), matching the Go CLI format.
pub fn list_sessions(cli: &crate::Cli) -> Result<()> {
    let cwd = cli.cwd.clone().unwrap_or(std::env::current_dir()?);
    let cwd = cwd
        .canonicalize()
        .with_context(|| format!("resolving working directory {}", cwd.display()))?;
    let mut options = pi_coding::ResourceManagerOptions::new(&cwd);
    options.headless = true;
    options.project_trust_override = if cli.approve {
        Some(true)
    } else if cli.no_approve {
        Some(false)
    } else {
        None
    };
    let resources =
        pi_coding::ResourceManager::new(options).context("loading settings and resources")?;
    let settings = resources.snapshot().settings.clone();
    let session_dir = crate::session_run::effective_session_dir(
        &cwd,
        cli.session_dir.as_deref(),
        settings.session_dir.as_deref(),
    )?;
    list_sessions_in(&cwd, &session_dir)
}

/// List saved sessions from an already-resolved session directory.
pub fn list_sessions_in(cwd: &Path, session_dir: &Path) -> Result<()> {
    let infos = pi_coding::list_sessions_in(cwd, Some(session_dir));
    if infos.is_empty() {
        println!("No sessions for this directory.");
        return Ok(());
    }
    for s in infos {
        println!(
            "{}  {}  {} msgs  {}",
            s.timestamp,
            s.id,
            s.messages,
            s.path.display()
        );
    }
    Ok(())
}

/// `rpi import-session SOURCE INPUT [--output PATH]`.
///
/// Converts an external session to native Pi v3 JSONL and prints the emitted
/// path plus the number of converted messages. Fails loudly on an unknown
/// source format or when no convertible messages are present.
pub fn import_session_command(
    cli: &crate::Cli,
    source: &str,
    input: &Path,
    output: Option<&Path>,
) -> Result<()> {
    let format = source
        .parse::<pi_coding::SourceSessionFormat>()
        .map_err(|e| anyhow::anyhow!("{e}"))
        .with_context(|| format!("unsupported source format {source:?}"))?;

    let imported = match output {
        Some(out) => pi_coding::import_session_to(format, input, out),
        None => {
            let cwd = cli.cwd.clone().unwrap_or(std::env::current_dir()?);
            let cwd = cwd
                .canonicalize()
                .with_context(|| format!("resolving working directory {}", cwd.display()))?;
            let mut options = pi_coding::ResourceManagerOptions::new(&cwd);
            options.headless = true;
            options.project_trust_override = if cli.approve {
                Some(true)
            } else if cli.no_approve {
                Some(false)
            } else {
                None
            };
            let resources = pi_coding::ResourceManager::new(options)
                .context("loading settings and resources")?;
            let settings = resources.snapshot().settings.clone();
            let session_dir = crate::session_run::effective_session_dir(
                &cwd,
                cli.session_dir.as_deref(),
                settings.session_dir.as_deref(),
            )?;
            pi_coding::import_session_into_directory(format, input, session_dir)
        }
    }
    .map_err(|e| anyhow::anyhow!("{e}"))?;

    println!(
        "imported {} ({} messages) -> {}",
        imported.source,
        imported.messages.len(),
        imported.path.display()
    );
    Ok(())
}

/// Resolve an automatic startup-resume input under the effective source policy
/// while retaining the selected native session root and environment-backed
/// foreign roots.
pub fn resolve_resume_for_startup(
    input: &str,
    preferred_cwd: Option<&Path>,
    session_dir: &Path,
    sources: &[pi_coding::SessionSourceKind],
) -> Result<crate::resume_catalog::ResumeSelectionResult> {
    let catalog = pi_coding::SessionCatalog::from_env()
        .map_err(anyhow::Error::new)?
        .with_native_session_root(session_dir);
    crate::resume_catalog::resolve_resume_selection(
        &catalog,
        &crate::resume_catalog::ResumeSelectionRequest::Input(input.to_owned()),
        preferred_cwd,
        sources,
    )
    .map_err(anyhow::Error::new)
}

/// Resolve a model spec to a model, surfacing a resolver warning on stderr
/// without aborting. Mirrors the Go CLI's handling of `ResolveModelPattern`.
///
/// Returns the resolved model and the thinking level parsed off a `:level`
/// suffix (empty string when none was present).
pub fn resolve_model_spec(spec: &str) -> Result<(pi_ai::Model, String)> {
    let resolved = pi_coding::resolve_model_pattern(spec).map_err(|e| anyhow::anyhow!("{e}"))?;
    if !resolved.warning.is_empty() {
        warn_line(&format!("Warning: {}", resolved.warning));
    }
    Ok((resolved.model, resolved.thinking_level))
}

/// `rpi export SESSION_PATH [--output PATH] [--jsonl]`.
///
/// Exports a session file to a self-contained HTML file (or current-branch
/// JSONL with `--jsonl`). No model, auth, or network access is required.
/// Prints the exact output path to stdout (no ANSI/diagnostics on stdout).
pub fn export_session_command(session: &Path, output: Option<&Path>, jsonl: bool) -> Result<()> {
    let path = if jsonl {
        pi_coding::export_session_jsonl(session, output)
    } else {
        pi_coding::export_session_html(session, output, &pi_coding::ExportOptions::default())
    }?;
    println!("{}", path.display());
    Ok(())
}

/// Validate the settings/resource graph and print a structured snapshot.
/// Diagnostics stay in JSON stdout for scripting; no ANSI is emitted.
pub fn reload_resources_command(cli: &crate::Cli) -> Result<()> {
    let cwd = cli.cwd.clone().unwrap_or(std::env::current_dir()?);
    let mut options = pi_coding::ResourceManagerOptions::new(&cwd);
    options.headless = true;
    options.project_trust_override = if cli.approve {
        Some(true)
    } else if cli.no_approve {
        Some(false)
    } else {
        None
    };
    let agent_dir = options.agent_dir.clone();
    let manager = pi_coding::ResourceManager::new(options)
        .map_err(|error| reload_display_error(&error, &cwd, &agent_dir))?;
    let snapshot = manager.snapshot();
    let present_path = |path: &Path| reload_display_path(path, &snapshot.cwd, &agent_dir);
    let trust = serde_json::json!({
        "decision": snapshot.trust.decision,
        "matchedPath": snapshot.trust.matched_path.as_deref().map(&present_path),
        "projectPath": present_path(&snapshot.trust.project_path),
    });
    let diagnostics = snapshot
        .diagnostics
        .iter()
        .map(|diagnostic| {
            serde_json::json!({
                "level": diagnostic.level,
                "message": reload_display_text(&diagnostic.message, &snapshot.cwd, &agent_dir),
                "path": diagnostic.path.as_deref().map(&present_path),
            })
        })
        .collect::<Vec<_>>();
    println!(
        "{}",
        serde_json::to_string(&serde_json::json!({
            "generation": snapshot.generation,
            "trust": trust,
            "contextFiles": snapshot.context_files.iter().map(|file| file.path.clone()).collect::<Vec<_>>(),
            "skills": snapshot.skills.iter().map(|skill| &skill.name).collect::<Vec<_>>(),
            "prompts": snapshot.prompts.iter().map(|prompt| &prompt.name).collect::<Vec<_>>(),
            "themes": snapshot.themes.iter().map(|theme| &theme.name).collect::<Vec<_>>(),
            "keybindingFiles": snapshot.keybinding_files.iter().map(|path| present_path(path)).collect::<Vec<_>>(),
            "diagnostics": diagnostics,
        }))?
    );
    Ok(())
}

fn reload_display_path(path: &Path, cwd: &Path, agent_dir: &Path) -> String {
    let relative = |root: &Path, placeholder: &str| {
        path.strip_prefix(root).ok().map(|suffix| {
            let suffix = suffix.to_string_lossy().replace('\\', "/");
            if suffix.is_empty() {
                placeholder.to_owned()
            } else {
                format!("{placeholder}/{suffix}")
            }
        })
    };
    relative(cwd, "<workspace>")
        .or_else(|| relative(agent_dir, "<agent-dir>"))
        .unwrap_or_else(|| {
            path.file_name()
                .and_then(|name| name.to_str())
                .map(|name| format!("<redacted>/{name}"))
                .unwrap_or_else(|| "<redacted>".to_owned())
        })
}

fn reload_display_text(text: &str, cwd: &Path, agent_dir: &Path) -> String {
    let mut roots = Vec::<(String, &'static str)>::with_capacity(4);
    for (root, placeholder) in [(cwd, "<workspace>"), (agent_dir, "<agent-dir>")] {
        let raw = root.to_string_lossy();
        for candidate in [raw.trim_end_matches(['/', '\\']), raw.as_ref()] {
            if !candidate.is_empty() && roots.iter().all(|(seen, _)| seen != candidate) {
                roots.push((candidate.to_owned(), placeholder));
            }
        }
        if let Ok(canonical) = root.canonicalize() {
            let canonical = canonical.to_string_lossy();
            if roots.iter().all(|(seen, _)| seen != canonical.as_ref()) {
                roots.push((canonical.into_owned(), placeholder));
            }
        }
    }
    roots.sort_by(|left, right| right.0.len().cmp(&left.0.len()));
    roots
        .into_iter()
        .fold(text.to_owned(), |display, (root, placeholder)| {
            replace_bounded_path_root(&display, &root, placeholder)
        })
}

fn replace_bounded_path_root(text: &str, root: &str, placeholder: &str) -> String {
    fn is_path_name_character(character: char) -> bool {
        character.is_alphanumeric() || matches!(character, '.' | '_' | '-')
    }

    let mut output = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(root) {
        let before = rest[..start].chars().next_back();
        let after = rest[start + root.len()..].chars().next();
        if before.is_none_or(|character| !is_path_name_character(character))
            && after.is_none_or(|character| !is_path_name_character(character))
        {
            output.push_str(&rest[..start]);
            output.push_str(placeholder);
            rest = &rest[start + root.len()..];
        } else {
            let keep = start + root.len();
            output.push_str(&rest[..keep]);
            rest = &rest[keep..];
        }
    }
    output.push_str(rest);
    output
}

const RELOAD_CONTEXT_DIAGNOSTIC_LIMIT: usize = 8;
const RELOAD_CONTEXT_PATH_LIMIT: usize = 240;
const RELOAD_CONTEXT_REASON_LIMIT: usize = 240;

fn reload_display_error(error: &anyhow::Error, cwd: &Path, agent_dir: &Path) -> anyhow::Error {
    let Some(diagnostics) = pi_coding::context_file_diagnostics(error) else {
        return anyhow::anyhow!(reload_display_text(&format!("{error:#}"), cwd, agent_dir));
    };
    let shown = diagnostics.len().min(RELOAD_CONTEXT_DIAGNOSTIC_LIMIT);
    let mut display = String::from("context file validation failed:");
    for diagnostic in diagnostics.iter().take(shown) {
        let path = pi_coding::redact::redact_bounded(
            &reload_display_path(&diagnostic.path, cwd, agent_dir),
            RELOAD_CONTEXT_PATH_LIMIT,
        );
        let reason = pi_coding::redact::redact_bounded(
            &reload_display_text(&diagnostic.message, cwd, agent_dir),
            RELOAD_CONTEXT_REASON_LIMIT,
        );
        display.push('\n');
        display.push_str("- ");
        display.push_str(&path);
        display.push_str(": ");
        display.push_str(&reason);
    }
    if diagnostics.len() > shown {
        display.push_str(&format!(
            "\n- {} more context file candidate(s) omitted",
            diagnostics.len() - shown
        ));
    }
    anyhow::anyhow!(display)
}

#[cfg(test)]
mod reload_path_tests {
    use super::{reload_display_error, reload_display_path, reload_display_text};
    use pi_coding::{ContextFileDiagnostic, ContextFileLoadError};
    use std::path::{Path, PathBuf};

    #[test]
    fn reload_paths_use_placeholders_without_absolute_prefixes() {
        assert_eq!(
            reload_display_path(
                Path::new("/tmp/project/nested/AGENTS.md"),
                Path::new("/tmp/project"),
                Path::new("/tmp/agent"),
            ),
            "<workspace>/nested/AGENTS.md",
        );
        assert_eq!(
            reload_display_path(
                Path::new("/tmp/agent/keybindings.json"),
                Path::new("/tmp/project"),
                Path::new("/tmp/agent"),
            ),
            "<agent-dir>/keybindings.json",
        );
        assert_eq!(
            reload_display_path(
                Path::new("/private/machine/secret.txt"),
                Path::new("/tmp/project"),
                Path::new("/tmp/agent"),
            ),
            "<redacted>/secret.txt",
        );
        assert_eq!(
            reload_display_text(
                "bad /tmp/project/AGENTS.md and /tmp/agent/settings.json",
                Path::new("/tmp/project"),
                Path::new("/tmp/agent"),
            ),
            "bad <workspace>/AGENTS.md and <agent-dir>/settings.json",
        );
        assert_eq!(
            reload_display_text(
                "/tmp/project2/file and /tmp/project/.pi/agent/settings.json",
                Path::new("/tmp/project"),
                Path::new("/tmp/project/.pi/agent"),
            ),
            "/tmp/project2/file and <agent-dir>/settings.json",
        );
    }

    #[test]
    fn reload_context_failure_lists_safe_actionable_typed_diagnostics() {
        let error = anyhow::Error::new(ContextFileLoadError {
            diagnostics: vec![
                ContextFileDiagnostic {
                    path: PathBuf::from("/tmp/project/nested/CLAUDE.md"),
                    message: "context file is not valid UTF-8 at /tmp/project/nested/CLAUDE.md"
                        .to_owned(),
                },
                ContextFileDiagnostic {
                    path: PathBuf::from("/tmp/agent/AGENTS.md"),
                    message: "context file is unreadable at /tmp/agent/AGENTS.md".to_owned(),
                },
            ],
        });
        let display =
            reload_display_error(&error, Path::new("/tmp/project"), Path::new("/tmp/agent"))
                .to_string();

        assert!(
            display.contains("<workspace>/nested/CLAUDE.md"),
            "{display}"
        );
        assert!(display.contains("not valid UTF-8"), "{display}");
        assert!(display.contains("<agent-dir>/AGENTS.md"), "{display}");
        assert!(!display.contains("/tmp/project"), "{display}");
        assert!(!display.contains("/tmp/agent"), "{display}");
    }
}
