//! CLI adapters for MCP server listing and config import (`rpi mcp list`,
//! `rpi mcp import`). Reading and parsing live in
//! `pi_coding::mcp_import` (lib-tested); this module only resolves paths,
//! persists through the settings manager, and prints reports.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use pi_coding::mcp_import::{
    McpImportFormat, McpImportOutcome, claude_desktop_config_path, cursor_mcp_config_path,
    merge_mcp_servers, parse_claude_config, parse_cursor_config,
};
use pi_coding::{SettingsManager, SettingsScope};

use crate::args::{McpCommand, McpImportSourceArg};

/// Dispatch `rpi mcp <command>`.
pub fn run(command: McpCommand, cwd: &Path) -> Result<()> {
    match command {
        McpCommand::List { local } => list_servers(cwd, local),
        McpCommand::Import {
            source,
            file,
            local,
            force,
        } => import_servers(cwd, source, file.as_deref(), local, force),
    }
}

/// Print configured MCP servers from the chosen settings scope. Disabled
/// entries are marked; env values are never printed (they may hold secrets).
pub fn list_servers(cwd: &Path, local: bool) -> Result<()> {
    let manager = settings_manager(cwd)?;
    let scope = scope_for(local);
    let settings = if local {
        manager.project_settings()
    } else {
        manager.global_settings()
    };
    let servers = &settings.mcp_servers;
    if servers.is_empty() {
        println!(
            "No MCP servers configured ({} scope). Add entries under `mcpServers` in \
             settings, or run `rpi mcp import` to pull them from a Claude Desktop or \
             Cursor config.",
            scope_label(scope)
        );
        return Ok(());
    }
    let enabled = servers.iter().filter(|server| !server.disabled).count();
    let disabled = servers.len() - enabled;
    let mut out = format!(
        "MCP servers ({} configured, {disabled} disabled, {} scope):",
        servers.len(),
        scope_label(scope)
    );
    for server in servers {
        let target = match server.transport {
            pi_coding::McpTransport::Stdio => {
                let mut command = server.command.clone().unwrap_or_default();
                if let Some(args) = &server.args {
                    command.push(' ');
                    command.push_str(&args.join(" "));
                }
                format!("stdio: {command}")
            }
            pi_coding::McpTransport::Sse => {
                format!("sse: {}", server.url.clone().unwrap_or_default())
            }
        };
        let flag = if server.disabled { " [disabled]" } else { "" };
        out.push_str(&format!("\n- {}{} ({target})", server.name, flag));
    }
    println!("{out}");
    Ok(())
}

/// Import MCP servers from a Claude Desktop or Cursor config into settings.
pub fn import_servers(
    cwd: &Path,
    source: Option<McpImportSourceArg>,
    file: Option<&Path>,
    local: bool,
    force: bool,
) -> Result<()> {
    let manager = settings_manager(cwd)?;
    let (format, path) = resolve_import_source(cwd, source, file)?;
    let contents = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    let entries = match format {
        McpImportFormat::Claude => parse_claude_config(&contents),
        McpImportFormat::Cursor => parse_cursor_config(&contents),
    }
    .with_context(|| format!("parsing {}", path.display()))?;

    let scope = scope_for(local);
    let outcome = if local {
        let mut outcome = None;
        manager
            .update_project(|settings| {
                outcome = Some(merge_mcp_servers(settings, entries, force));
            })
            .context("writing project settings")?;
        outcome.expect("merge closure ran")
    } else {
        let mut outcome = None;
        manager
            .update_global(|settings| {
                outcome = Some(merge_mcp_servers(settings, entries, force));
            })
            .context("writing global settings")?;
        outcome.expect("merge closure ran")
    };
    print_import_report(&format, &path, scope, &outcome);
    Ok(())
}

/// Resolve which config file to read and which format parses it.
fn resolve_import_source(
    cwd: &Path,
    source: Option<McpImportSourceArg>,
    file: Option<&Path>,
) -> Result<(McpImportFormat, PathBuf)> {
    if let Some(path) = file {
        let format = match source {
            Some(McpImportSourceArg::Claude) => McpImportFormat::Claude,
            Some(McpImportSourceArg::Cursor) => McpImportFormat::Cursor,
            Some(McpImportSourceArg::Auto) | None => {
                let is_cursor = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.eq_ignore_ascii_case("mcp.json"));
                if is_cursor {
                    McpImportFormat::Cursor
                } else {
                    McpImportFormat::Claude
                }
            }
        };
        return Ok((format, path.to_path_buf()));
    }
    match source.unwrap_or(McpImportSourceArg::Auto) {
        McpImportSourceArg::Claude => {
            let path = claude_desktop_config_path()
                .context("could not determine the Claude Desktop config path (HOME is unset)")?;
            Ok((McpImportFormat::Claude, path))
        }
        McpImportSourceArg::Cursor => Ok((McpImportFormat::Cursor, cursor_mcp_config_path(cwd))),
        McpImportSourceArg::Auto => {
            if let Some(path) = claude_desktop_config_path() {
                if path.exists() {
                    return Ok((McpImportFormat::Claude, path));
                }
            }
            let cursor_path = cursor_mcp_config_path(cwd);
            if cursor_path.exists() {
                return Ok((McpImportFormat::Cursor, cursor_path));
            }
            bail!(
                "no Claude Desktop config at the standard path and no {} found; \
                 use `--source claude|cursor` or `--file PATH`",
                cursor_path.display()
            );
        }
    }
}

fn print_import_report(
    format: &McpImportFormat,
    path: &Path,
    scope: SettingsScope,
    outcome: &McpImportOutcome,
) {
    println!(
        "Imported MCP servers from {} config {} ({} scope):",
        format.label(),
        path.display(),
        scope_label(scope)
    );
    for name in &outcome.imported {
        println!("  added {name}");
    }
    for name in &outcome.replaced {
        println!("  replaced {name} (--force)");
    }
    for name in &outcome.skipped {
        println!(
            "  kept existing {name} (use --force to replace it)",
        );
    }
    for name in &outcome.disabled {
        println!("  disabled {name}");
    }
    for rejected in &outcome.rejected {
        println!("  rejected {rejected}");
    }
    if outcome.is_empty() {
        println!("  (nothing to import)");
    }
}

fn settings_manager(cwd: &Path) -> Result<SettingsManager> {
    let agent_dir = pi_coding::agent_dir_path();
    SettingsManager::load_phase_one(cwd, &agent_dir).context("loading settings")
}

fn scope_for(local: bool) -> SettingsScope {
    if local {
        SettingsScope::Project
    } else {
        SettingsScope::Global
    }
}

/// Human label for a settings scope (used in CLI output).
fn scope_label(scope: SettingsScope) -> &'static str {
    match scope {
        SettingsScope::Global => "global",
        SettingsScope::Project => "project",
    }
}
