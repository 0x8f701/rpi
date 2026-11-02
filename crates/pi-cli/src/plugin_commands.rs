//! CLI adapters for the rpi plugin marketplace (`rpi plugin list/install/
//! remove/update`). Output is intentionally plain so it stays safe when
//! stdout is captured or piped.

use std::path::Path;

use anyhow::{Context, Result, anyhow};
use pi_coding::plugin::{
    DEFAULT_MARKETPLACE_INDEX_URL, MarketplaceIndex, PLUGIN_MARKETPLACE_SETTING,
    PluginHttpExecutors, PluginMarketplace, PluginRuntime, compare_versions,
};
use pi_coding::{SettingsManager, TrustStore};

/// List installed plugins (name/version/runtime/trusted).
pub fn list_plugins(cwd: &Path, http: PluginHttpExecutors) -> Result<()> {
    let (marketplace, trust_store, _) = marketplace(cwd, None, http)?;
    let (plugins, problems) = marketplace.list(&trust_store)?;
    if plugins.is_empty() && problems.is_empty() {
        println!("No plugins installed.");
        println!("Install one with `rpi plugin install <directory|archive|owner/repo[@ref]|git URL>`.");
    }
    for plugin in plugins {
        println!(
            "{}  {}  {}  {}",
            plugin.name,
            plugin.version,
            plugin.runtime.label(),
            if plugin.trusted {
                "trusted"
            } else {
                "untrusted"
            }
        );
    }
    for problem in problems {
        eprintln!("warning: {problem}");
    }
    Ok(())
}

/// Install a plugin from a local directory, a local or remote tarball, an
/// owner/repo GitHub reference (`@ref` optional), an npm reference, or a git URL.
pub async fn install_plugin(
    source: &str,
    cwd: &Path,
    process: pi_coding::GovernedProcessContext,
    http: PluginHttpExecutors,
) -> Result<()> {
    let (marketplace, trust_store, _) = marketplace(cwd, Some(process), http)?;
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
pub fn remove_plugin(name: &str, cwd: &Path, http: PluginHttpExecutors) -> Result<()> {
    let (marketplace, trust_store, _) = marketplace(cwd, None, http)?;
    marketplace
        .remove(name, &trust_store)
        .with_context(|| format!("removing plugin {name}"))?;
    println!("removed plugin {name}");
    Ok(())
}

/// Update one installed plugin from the marketplace index.
pub async fn update_plugin(
    name: &str,
    cwd: &Path,
    process: pi_coding::GovernedProcessContext,
    http: PluginHttpExecutors,
) -> Result<()> {
    let (marketplace, trust_store, index_source) = marketplace(cwd, Some(process), http)?;
    let index = fetch_index_actionable(&marketplace, &index_source).await?;
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
pub async fn list_updates(cwd: &Path, http: PluginHttpExecutors) -> Result<()> {
    let (marketplace, trust_store, index_source) = marketplace(cwd, None, http)?;
    let index = fetch_index_actionable(&marketplace, &index_source).await?;
    let (installed, _) = marketplace.list(&trust_store)?;
    let updates = installed.into_iter().filter_map(|plugin| {
        let entry = index.entry(&plugin.name)?;
        (compare_versions(&entry.version, &plugin.version).is_gt()).then_some((plugin, entry))
    });
    let mut updates = updates.peekable();
    if updates.peek().is_none() {
        println!("All plugins are up to date.");
        return Ok(());
    }
    for (plugin, entry) in updates {
        println!(
            "{}  {} -> {}  ({})",
            plugin.name, plugin.version, entry.version, entry.repo
        );
    }
    Ok(())
}

/// Dispatch one `rpi plugin` subcommand.
pub async fn run(
    command: crate::args::PluginCommand,
    cwd: &Path,
    process: pi_coding::GovernedProcessContext,
    http: PluginHttpExecutors,
) -> Result<()> {
    match command {
        crate::args::PluginCommand::List { updates: false } => list_plugins(cwd, http),
        crate::args::PluginCommand::List { updates: true } => list_updates(cwd, http).await,
        crate::args::PluginCommand::Install { source } => {
            install_plugin(&source, cwd, process, http).await
        }
        crate::args::PluginCommand::Remove { name } => remove_plugin(&name, cwd, http),
        crate::args::PluginCommand::Update { name } => {
            update_plugin(&name, cwd, process, http).await
        }
    }
}

fn marketplace(
    cwd: &Path,
    process: Option<pi_coding::GovernedProcessContext>,
    http: PluginHttpExecutors,
) -> Result<(PluginMarketplace, TrustStore, String)> {
    let agent_dir = pi_coding::agent_dir_path();
    let settings = SettingsManager::load_phase_one(cwd, &agent_dir)
        .context("loading settings for plugin marketplace")?;
    let index_source = settings
        .global_settings()
        .extra
        .get(PLUGIN_MARKETPLACE_SETTING)
        .and_then(serde_json::Value::as_str)
        .unwrap_or(DEFAULT_MARKETPLACE_INDEX_URL)
        .to_owned();
    let marketplace = PluginMarketplace::new(&agent_dir)?
        .with_operator_cwd(cwd)
        .with_http_executors(http);
    let marketplace = match process {
        Some(process) => marketplace.with_process_context(process),
        None => marketplace,
    };
    Ok((marketplace, TrustStore::new(&agent_dir), index_source))
}

async fn fetch_index_actionable(
    marketplace: &PluginMarketplace,
    source: &str,
) -> Result<MarketplaceIndex> {
    marketplace.fetch_index(source).await.map_err(|error| {
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
        let agent = tempfile::tempdir().expect("agent dir");
        let marketplace = PluginMarketplace::new(agent.path()).expect("marketplace");
        let error = fetch_index_actionable(&marketplace, "file:///nonexistent/never/index.json")
            .await
            .expect_err("missing index must fail");
        let message = format!("{error:#}");
        assert!(
            message.contains("cannot fetch the plugin marketplace index"),
            "{message}"
        );
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
        let agent = tempfile::tempdir()?;
        let marketplace = PluginMarketplace::new(agent.path())?;
        let index = fetch_index_actionable(&marketplace, path.to_str().unwrap()).await?;
        assert_eq!(index.entry("demo").expect("entry").version, "1.0.0");
        Ok(())
    }
}
