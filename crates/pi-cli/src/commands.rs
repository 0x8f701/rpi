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
pub async fn list_models(filter: Option<&str>) -> Result<()> {
    crate::models_config::load_custom_models()?;
    if let Err(error) = crate::models_config::load_radius_catalog(!crate::session_run::offline()).await
        && crate::session_run::offline()
    {
        warn_line(&format!("Warning: Could not load Radius catalog: {error:#}"));
    }
    let manager = pi_coding::LlamaManager::default();
    if manager.effective_settings()?.is_some() && !crate::session_run::offline() {
        let refreshed = manager.refresh_catalog().await?;
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
        if filter.is_none_or(|needle| model.id.contains(needle) || model.provider.contains(needle)) {
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
pub async fn refresh_model_catalogs() -> Result<()> {
    crate::models_config::load_custom_models()?;
    crate::models_config::load_radius_catalog(true).await?;
    let manager = pi_coding::LlamaManager::default();
    if manager.effective_settings()?.is_some() {
        manager.refresh_catalog().await?;
    }
    println!("Model catalogs refreshed");
    Ok(())
}

/// List saved sessions for `cwd` (newest first), matching the Go CLI format.
pub fn list_sessions(cwd: &Path) -> Result<()> {
    let infos = pi_coding::list_sessions(cwd);
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

/// `pi import-session SOURCE INPUT [--output PATH]`.
///
/// Converts an external session to native Pi v3 JSONL and prints the emitted
/// path plus the number of converted messages. Fails loudly on an unknown
/// source format or when no convertible messages are present.
pub fn import_session_command(source: &str, input: &Path, output: Option<&Path>) -> Result<()> {
    let format = source
        .parse::<pi_coding::SourceSessionFormat>()
        .map_err(|e| anyhow::anyhow!("{e}"))
        .with_context(|| format!("unsupported source format \"{source}\""))?;

    let imported = match output {
        Some(out) => pi_coding::import_session_to(format, input, out),
        None => pi_coding::import_session(format, input),
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

/// Convert a Codex session (path or source id) to native Pi v3 JSONL and
/// return the emitted session path, ready to be loaded for resume.
///
/// Used by `pi --resume-codex PATH_OR_ID`.
pub fn import_codex_for_resume(input: &str) -> Result<std::path::PathBuf> {
    let path = std::path::Path::new(input);
    let imported = pi_coding::import_session(pi_coding::SourceSessionFormat::Codex, path)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    if imported.messages.is_empty() {
        warn_line(&format!(
            "warning: imported session {} has no convertible messages",
            imported.id
        ));
    }
    Ok(imported.path)
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

/// `pi export SESSION_PATH [--output PATH] [--jsonl]`.
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
    let manager = pi_coding::ResourceManager::new(options)?;
    let snapshot = manager.snapshot();
    println!(
        "{}",
        serde_json::to_string(&serde_json::json!({
            "generation": snapshot.generation,
            "trust": snapshot.trust,
            "contextFiles": snapshot.context_files.iter().map(|file| &file.path).collect::<Vec<_>>(),
            "skills": snapshot.skills.iter().map(|skill| &skill.name).collect::<Vec<_>>(),
            "prompts": snapshot.prompts.iter().map(|prompt| &prompt.name).collect::<Vec<_>>(),
            "themes": snapshot.themes.iter().map(|theme| &theme.name).collect::<Vec<_>>(),
            "keybindingFiles": snapshot.keybinding_files,
            "diagnostics": snapshot.diagnostics,
        }))?
    );
    Ok(())
}
