//! CLI adapters for Pi package install, removal, listing, and reconciliation.

use std::path::Path;

use anyhow::{Context, Result, bail};
use pi_coding::packages::{PackageManager, PackageOperation, PackageScope};
use pi_coding::{SettingsManager, TrustStore, resolve_project_trust};

/// Install a local or git package and persist it to global or project settings.
pub fn install_package(source: &str, local: bool, cwd: &Path) -> Result<()> {
    let manager = package_manager(cwd)?;
    let scope = scope_for(local);
    let operation = manager
        .install(source, scope)
        .with_context(|| format!("installing package {source}"))?;
    print_operation("installed", &operation);
    Ok(())
}

/// Remove a local or git package from global or project settings.
pub fn remove_package(source: &str, local: bool, cwd: &Path) -> Result<()> {
    let manager = package_manager(cwd)?;
    let scope = scope_for(local);
    if !manager
        .remove(source, scope)
        .with_context(|| format!("removing package {source}"))?
    {
        bail!("no matching {} package found for {source}", scope.label());
    }
    println!("removed {source} ({})", scope.label());
    Ok(())
}

/// Print configured packages from settings. Output is intentionally plain so
/// it remains safe when stdout is captured or piped.
pub fn list_packages(cwd: &Path) -> Result<()> {
    let manager = package_manager(cwd)?;
    let packages = manager.list()?;
    if packages.is_empty() {
        println!("No packages configured.");
        return Ok(());
    }
    let updates = if crate::session_run::offline() {
        Vec::new()
    } else {
        manager.check_available_updates().unwrap_or_default()
    };
    for package in packages {
        let status = if !package.supported {
            "unsupported"
        } else if package.installed_path.is_some() {
            "installed"
        } else {
            "missing"
        };
        let pinned = if package.pinned { " pinned" } else { "" };
        let update = updates
            .iter()
            .find(|update| update.scope == package.scope && update.source == package.source)
            .map_or("", |_| " update-available");
        match package.installed_path {
            Some(path) => println!(
                "{}  {}  {status}{pinned}{update}  {}",
                package.scope.label(),
                package.source,
                path.display()
            ),
            None => println!(
                "{}  {}  {status}{pinned}{update}",
                package.scope.label(),
                package.source
            ),
        }
    }
    Ok(())
}

/// Reconcile every configured package. npm entries produce the backend's clear
/// deferred error and are never written to package state.
pub fn update_packages(cwd: &Path) -> Result<()> {
    let operations = package_manager(cwd)?.update_all()?;
    if operations.is_empty() {
        println!("No packages to update.");
        return Ok(());
    }
    for operation in operations {
        print_operation("updated", &operation);
    }
    Ok(())
}

/// Reconcile one configured git or local package by its identity.
pub fn update_package(source: &str, cwd: &Path) -> Result<()> {
    let operations = package_manager(cwd)?
        .update_one(source)
        .with_context(|| format!("updating package {source}"))?;
    for operation in operations {
        print_operation("updated", &operation);
    }
    Ok(())
}

fn package_manager(cwd: &Path) -> Result<PackageManager> {
    let agent_dir = pi_coding::agent_dir_path();
    let settings = SettingsManager::load_phase_one(cwd, &agent_dir)
        .context("loading global settings for package trust policy")?;
    let default = settings
        .global_settings()
        .default_project_trust
        .unwrap_or_default();
    let stored = resolve_project_trust(&TrustStore::new(&agent_dir), cwd, None, default, true)
        .context("resolving project trust for package operations")?;
    let project_trusted = stored.is_trusted();
    PackageManager::with_agent_dir(cwd, agent_dir, project_trusted)
}

const fn scope_for(local: bool) -> PackageScope {
    if local {
        PackageScope::Project
    } else {
        PackageScope::Global
    }
}

fn print_operation(action: &str, operation: &PackageOperation) {
    match &operation.revision {
        Some(revision) => println!(
            "{action} {} ({}) at {} [{}]",
            operation.source,
            operation.scope.label(),
            operation.root.display(),
            revision
        ),
        None => println!(
            "{action} {} ({}) at {}",
            operation.source,
            operation.scope.label(),
            operation.root.display()
        ),
    }
}
