//! CLI adapters for the rpi plugin marketplace (`rpi plugin list/install/
//! remove/update`). Output is intentionally plain so it stays safe when
//! stdout is captured or piped.

use std::path::Path;

use anyhow::{Context, Result, anyhow};
use pi_coding::plugin::{
    DEFAULT_MARKETPLACE_INDEX_URL, PLUGIN_MARKETPLACE_SETTING, MarketplaceIndex,
    PluginMarketplace, PluginRuntime,
};
use pi_coding::{SettingsManager, TrustStore};

/// List installed plugins (name/version/runtime/trusted).
pub fn list_plugins(cwd: &Path) -> Result<()> {
    let (marketplace, trust_store, _) = marketplace(cwd)?;
    let (plugins, problems) = marketplace.list(&trust_store)?;
    if plugins.is_empty() && problems.is_empty() {
        println!("No plugins installed.");
        println!(
            "Install one with `rpi plugin install <directory|archive|owner/repo|git URL>`."
        );
    }
    for plugin in plugins {
        println!(
            "{}  {}  {}  {}",
            plugin.name,
            plugin.version,
            plugin.runtime.label(),
            if plugin.trusted { "trusted" } else { "untrusted" }
        );
    }
    for problem in problems {
        eprintln!("warning: {problem}");
    }
    Ok(())
}

/// Install a plugin from a local directory, a local or remote tarball, an
/// owner/repo GitHub reference, an npm reference, or a git URL.
pub async fn install_plugin(source: &str, cwd: &Path) -> Result<()> {
    let (marketplace, trust_store, _) = marketplace(cwd)?;
    let installed = marketplace
        .install(source, &trust_store)
        .await
        .with_context(|| {
            format!(
                "installing plugin from {}",
                pi_coding::plugin::redact_url_credentials(source)
            )
        })?;
    println!(
        "installed {} {} ({} runtime) at {}; trusted and loadable",
        installed.name,
        installed.version,
        installed.runtime.label(),
        installed.path.display()
    );
    Ok(())
}

/// Remove an installed plugin.
pub fn remove_plugin(name: &str, cwd: &Path) -> Result<()> {
    let (marketplace, trust_store, _) = marketplace(cwd)?;
    marketplace
        .remove(name, &trust_store)
        .with_context(|| format!("removing plugin {name}"))?;
    println!("removed plugin {name}");
    Ok(())
}

/// Update one installed plugin from the marketplace index.
pub async fn update_plugin(name: &str, cwd: &Path) -> Result<()> {
    let (marketplace, trust_store, index_source) = marketplace(cwd)?;
    let index = fetch_index_actionable(&index_source).await?;
    let updated = marketplace
        .update(name, &index, &trust_store)
        .await
        .with_context(|| format!("updating plugin {name}"))?;
    println!(
        "updated {} to {} at {}",
        updated.name,
        updated.version,
        updated.path.display()
    );
    Ok(())
}

/// Print every installed plugin with a newer version in the marketplace index.
pub async fn list_updates(cwd: &Path) -> Result<()> {
    let (marketplace, _, index_source) = marketplace(cwd)?;
    let index = fetch_index_actionable(&index_source).await?;
    let updates = marketplace.available_updates(&index)?;
    if updates.is_empty() {
        println!("All plugins are up to date.");
        return Ok(());
    }
    for update in updates {
        println!(
            "{}  {} -> {}  ({})",
            update.name, update.current, update.available, update.repo
        );
    }
    Ok(())
}

/// Dispatch one `rpi plugin` subcommand.
pub async fn run(command: crate::args::PluginCommand, cwd: &Path) -> Result<()> {
    match command {
        crate::args::PluginCommand::List { updates: false } => list_plugins(cwd),
        crate::args::PluginCommand::List { updates: true } => list_updates(cwd).await,
        crate::args::PluginCommand::Install { source } => install_plugin(&source, cwd).await,
        crate::args::PluginCommand::Remove { name } => remove_plugin(&name, cwd),
        crate::args::PluginCommand::Update { name } => update_plugin(&name, cwd).await,
    }
}

fn marketplace(cwd: &Path) -> Result<(PluginMarketplace, TrustStore, String)> {
    let agent_dir = pi_coding::agent_dir_path();
    let settings = SettingsManager::load_phase_one(cwd, &agent_dir)
        .context("loading settings for plugin marketplace")?;
    let index_source = settings
        .global_settings()
        .extra
        .get(PLUGIN_MARKETPLACE_SETTING)
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| DEFAULT_MARKETPLACE_INDEX_URL.to_owned());
    Ok((
        PluginMarketplace::new(&agent_dir),
        TrustStore::new(&agent_dir),
        index_source,
    ))
}

async fn fetch_index_actionable(source: &str) -> Result<MarketplaceIndex> {
    pi_coding::plugin::fetch_index(source)
        .await
        .map_err(|error| {
            anyhow!(
                "cannot fetch the plugin marketplace index from {source}: {error:#}\n\
                 check the network connection, or set the `{PLUGIN_MARKETPLACE_SETTING}` \
                 setting in settings.json to a reachable index URL or a local index file path"
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn plugin_runtime_labels() {
        assert_eq!(PluginRuntime::Process.label(), "process");
        assert_eq!(PluginRuntime::QuickJs.label(), "quickjs");
    }

    #[tokio::test]
    async fn offline_index_failure_is_actionable() {
        let error = fetch_index_actionable("file:///nonexistent/never/index.json")
            .await
            .expect_err("missing index must fail");
        let message = format!("{error:#}");
        assert!(message.contains("cannot fetch the plugin marketplace index"), "{message}");
        assert!(message.contains(PLUGIN_MARKETPLACE_SETTING), "{message}");
        assert!(message.contains("local index file"), "{message}");
    }

    #[tokio::test]
    async fn local_index_file_fetches_and_parses() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("index.json");
        std::fs::write(
            &path,
            serde_json::to_vec_pretty(&[json!({
                "name": "demo",
                "repo": "owner/demo",
                "version": "1.0.0",
                "description": "demo plugin",
            })])?,
        )?;
        let index = fetch_index_actionable(path.to_str().unwrap()).await?;
        assert_eq!(index.entry("demo").expect("entry").version, "1.0.0");
        Ok(())
    }
}
