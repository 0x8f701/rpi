//! Import MCP server definitions from Claude Desktop and Cursor configs.
//!
//! Both tools store servers as a *map* of name → entry, unlike rpi's
//! `Settings.mcp_servers` array of `{ name, transport, ... }` objects. This
//! module translates the native shapes into [`McpServerConfig`] entries:
//!
//! - **Claude Desktop** (`claude_desktop_config.json` at the platform config
//!   path): entries are always stdio (`command`/`args`/`env`); the top-level
//!   `disabledMCPServers` name list maps onto the per-entry `disabled` flag.
//! - **Cursor** (`.cursor/mcp.json` in the project): entries carry a `type`
//!   (`stdio` | `sse` | `http`) plus `command`/`args`/`env`/`url` and a
//!   per-entry `disabled` boolean — the shape rpi mirrors. `http` entries
//!   import as the sse transport (this build's client transport is stdio;
//!   sse entries parse and round-trip but report the limitation when used).
//!
//! Unknown entry fields are retained in `extra` so a config survives a
//! settings write losslessly. [`merge_mcp_servers`] validates every entry
//! (disabled entries are exempt from transport requirements — they never
//! spawn) and never overwrites an existing server unless `force` is set.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value};

use crate::settings::{McpServerConfig, McpTransport, Settings};

/// The external config format an import reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum McpImportFormat {
    /// Claude Desktop's `claude_desktop_config.json`.
    Claude,
    /// Cursor's `.cursor/mcp.json`.
    Cursor,
}

impl McpImportFormat {
    /// Human label for reports and error messages.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Claude => "Claude Desktop",
            Self::Cursor => "Cursor",
        }
    }
}

/// Per-entry result of a merge, for reporting to the user.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct McpImportOutcome {
    /// Server names newly added to settings.
    pub imported: Vec<String>,
    /// Server names replaced because the same name already existed and
    /// `force` was set.
    pub replaced: Vec<String>,
    /// Server names left untouched because they already existed and `force`
    /// was not set.
    pub skipped: Vec<String>,
    /// Server names imported with `disabled: true`.
    pub disabled: Vec<String>,
    /// Invalid entries rejected with their reason (`"name: reason"`).
    pub rejected: Vec<String>,
}

impl McpImportOutcome {
    /// True when nothing was merged or rejected (empty source config).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.imported.is_empty()
            && self.replaced.is_empty()
            && self.skipped.is_empty()
            && self.disabled.is_empty()
            && self.rejected.is_empty()
    }
}

/// Resolve Claude Desktop's platform config path
/// (`claude_desktop_config.json`). Honors `CLAUDE_CONFIG_DIR` when set;
/// otherwise the platform default:
/// - macOS: `~/Library/Application Support/Claude/`
/// - Windows: `%APPDATA%\Claude\` (from `USERPROFILE`)
/// - other Unix: `~/.config/Claude/`
///
/// `None` when no home directory can be determined.
#[must_use]
pub fn claude_desktop_config_path() -> Option<PathBuf> {
    let base = std::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
            let mut base = PathBuf::from(home);
            if cfg!(target_os = "macos") {
                base.push("Library/Application Support/Claude");
            } else if cfg!(target_os = "windows") {
                base.push("AppData/Roaming/Claude");
            } else {
                base.push(".config/Claude");
            }
            Some(base)
        });
    base.map(|base| base.join("claude_desktop_config.json"))
}

/// Cursor's project MCP config: `<cwd>/.cursor/mcp.json`.
#[must_use]
pub fn cursor_mcp_config_path(cwd: &Path) -> PathBuf {
    cwd.join(".cursor").join("mcp.json")
}

/// Parse a Claude Desktop config document (`claude_desktop_config.json`)
/// into server entries. Entries named in the top-level `disabledMCPServers`
/// list arrive with `disabled: true`.
pub fn parse_claude_config(contents: &str) -> Result<Vec<McpServerConfig>> {
    let document: Value = serde_json::from_str(contents)
        .context("Claude Desktop config is not valid JSON")?;
    let servers = document
        .get("mcpServers")
        .and_then(Value::as_object)
        .context("Claude Desktop config has no `mcpServers` object")?;
    let disabled_names: std::collections::BTreeSet<String> = document
        .get("disabledMCPServers")
        .and_then(Value::as_array)
        .map(|names| {
            names
                .iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();
    let mut out = Vec::with_capacity(servers.len());
    for (name, entry) in servers {
        let mut config = entry_to_config(name, entry)?;
        if disabled_names.contains(name) {
            config.disabled = true;
        }
        out.push(config);
    }
    Ok(out)
}

/// Parse a Cursor `.cursor/mcp.json` document into server entries. Per-entry
/// `disabled` booleans map directly onto [`McpServerConfig::disabled`];
/// `type: "http"` entries import as the sse transport.
pub fn parse_cursor_config(contents: &str) -> Result<Vec<McpServerConfig>> {
    let document: Value =
        serde_json::from_str(contents).context("Cursor mcp.json is not valid JSON")?;
    let servers = document
        .get("mcpServers")
        .and_then(Value::as_object)
        .context("Cursor mcp.json has no `mcpServers` object")?;
    let mut out = Vec::with_capacity(servers.len());
    for (name, entry) in servers {
        out.push(entry_to_config(name, entry)?);
    }
    Ok(out)
}

/// Translates one map entry (`name` → entry object) into an
/// [`McpServerConfig`]. `type`/`transport` select the transport (default
/// stdio, matching Claude); unknown fields are retained in `extra`.
fn entry_to_config(name: &str, entry: &Value) -> Result<McpServerConfig> {
    let entry = entry
        .as_object()
        .with_context(|| format!("mcpServers entry `{name}` must be an object"))?;
    let transport = match entry
        .get("type")
        .or_else(|| entry.get("transport"))
        .and_then(Value::as_str)
    {
        None | Some("stdio") => McpTransport::Stdio,
        Some("sse") | Some("http") | Some("streamable-http") => McpTransport::Sse,
        Some(other) => bail!("mcpServers entry `{name}` has unsupported transport `{other}`"),
    };
    let command = entry
        .get("command")
        .and_then(Value::as_str)
        .map(String::from);
    let args = entry
        .get("args")
        .and_then(Value::as_array)
        .map(|args| {
            args.iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect()
        });
    let env = entry.get("env").and_then(Value::as_object).map(|env| {
        env.iter()
            .map(|(key, value)| {
                (
                    key.clone(),
                    value
                        .as_str()
                        .map(String::from)
                        .unwrap_or_else(|| value.to_string()),
                )
            })
            .collect::<BTreeMap<String, String>>()
    });
    let url = entry.get("url").and_then(Value::as_str).map(String::from);
    let disabled = entry
        .get("disabled")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    // Retain everything unknown so the config round-trips losslessly.
    let mut extra = Map::new();
    for (key, value) in entry {
        if !matches!(
            key.as_str(),
            "type" | "transport" | "command" | "args" | "env" | "url" | "disabled"
        ) {
            extra.insert(key.clone(), value.clone());
        }
    }
    Ok(McpServerConfig {
        name: name.to_owned(),
        disabled,
        transport,
        command,
        args,
        url,
        env,
        extra,
    })
}

/// Validates one imported entry with the same rules as the settings
/// validator, except that disabled entries are exempt from transport
/// requirements (they never spawn; the settings validator mirrors this).
fn validate_imported(server: &McpServerConfig) -> Result<()> {
    if server.name.trim().is_empty() {
        bail!("name must not be empty");
    }
    if server.disabled {
        return Ok(());
    }
    match server.transport {
        McpTransport::Stdio => {
            if server
                .command
                .as_ref()
                .is_none_or(|command| command.trim().is_empty())
            {
                bail!("stdio transport requires a non-empty command");
            }
            if server.url.is_some() {
                bail!("stdio transport must not set url (url is for the sse transport)");
            }
        }
        McpTransport::Sse => {
            let Some(url) = server.url.as_deref() else {
                bail!("sse transport requires a non-empty url");
            };
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                bail!("sse transport url must start with http:// or https://");
            }
            if server.command.is_some() {
                bail!("sse transport must not set command (command is for the stdio transport)");
            }
        }
    }
    Ok(())
}

/// Merges imported `entries` into `settings.mcp_servers`.
///
/// Each entry is validated individually; invalid entries are rejected into
/// the outcome instead of failing the whole import. Existing servers are
/// never overwritten unless `force` is true (matching names are skipped and
/// reported).
pub fn merge_mcp_servers(
    settings: &mut Settings,
    entries: Vec<McpServerConfig>,
    force: bool,
) -> McpImportOutcome {
    let mut outcome = McpImportOutcome::default();
    for server in entries {
        if let Err(error) = validate_imported(&server) {
            outcome
                .rejected
                .push(format!("{}: {error:#}", server.name));
            continue;
        }
        if server.disabled {
            outcome.disabled.push(server.name.clone());
        }
        match settings
            .mcp_servers
            .iter()
            .position(|existing| existing.name == server.name)
        {
            Some(index) if force => {
                settings.mcp_servers[index] = server;
                outcome.replaced.push(settings.mcp_servers[index].name.clone());
            }
            Some(_) => outcome.skipped.push(server.name.clone()),
            None => {
                outcome.imported.push(server.name.clone());
                settings.mcp_servers.push(server);
            }
        }
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    use crate::settings::{McpTransport, Settings, SettingsScope};

    const CLAUDE_FIXTURE: &str = r#"{
        "mcpServers": {
            "filesystem": {
                "command": "npx",
                "args": ["-y", "@modelcontextprotocol/server-filesystem"]
            },
            "github": {
                "command": "npx",
                "args": ["-y", "@modelcontextprotocol/server-github"],
                "env": { "GITHUB_TOKEN": "$GITHUB_TOKEN" }
            },
            "legacy": {
                "command": "python",
                "args": ["-m", "legacy_server"],
                "timeout": 10
            }
        },
        "disabledMCPServers": ["github"]
    }"#;

    const CURSOR_FIXTURE: &str = r#"{
        "mcpServers": {
            "local": { "type": "stdio", "command": "npx", "args": ["-y", "local-pkg"], "disabled": false },
            "remote": { "type": "sse", "url": "https://mcp.example.com/remote", "disabled": true },
            "httpy": { "type": "http", "url": "http://localhost:9000/mcp" }
        }
    }"#;

    #[test]
    fn claude_config_parses_entries_and_disabled_list() {
        let servers = parse_claude_config(CLAUDE_FIXTURE).expect("parse claude fixture");
        assert_eq!(servers.len(), 3);

        let filesystem = servers
            .iter()
            .find(|server| server.name == "filesystem")
            .expect("filesystem");
        assert_eq!(filesystem.transport, McpTransport::Stdio);
        assert_eq!(filesystem.command.as_deref(), Some("npx"));
        assert!(!filesystem.disabled, "only disabledMCPServers names are disabled");

        let github = servers
            .iter()
            .find(|server| server.name == "github")
            .expect("github");
        assert!(github.disabled, "github is listed in disabledMCPServers");
        assert_eq!(
            github.env.as_ref().and_then(|env| env.get("GITHUB_TOKEN")),
            Some(&"$GITHUB_TOKEN".to_owned()),
            "env references survive parsing for the settings write (expanded only at runtime)"
        );

        // Unknown fields are retained so the config round-trips.
        let legacy = servers
            .iter()
            .find(|server| server.name == "legacy")
            .expect("legacy");
        assert_eq!(
            legacy.extra.get("timeout").and_then(Value::as_i64),
            Some(10)
        );
    }

    #[test]
    fn cursor_config_parses_transports_and_disabled_flags() {
        let servers = parse_cursor_config(CURSOR_FIXTURE).expect("parse cursor fixture");
        assert_eq!(servers.len(), 3);

        let local = servers
            .iter()
            .find(|server| server.name == "local")
            .expect("local");
        assert_eq!(local.transport, McpTransport::Stdio);
        assert_eq!(local.command.as_deref(), Some("npx"));
        assert!(!local.disabled);

        let remote = servers
            .iter()
            .find(|server| server.name == "remote")
            .expect("remote");
        assert_eq!(remote.transport, McpTransport::Sse);
        assert_eq!(remote.url.as_deref(), Some("https://mcp.example.com/remote"));
        assert!(remote.disabled, "per-entry disabled maps directly");

        // Cursor `type: http` imports as sse (transport limitation documented).
        let httpy = servers
            .iter()
            .find(|server| server.name == "httpy")
            .expect("httpy");
        assert_eq!(httpy.transport, McpTransport::Sse);
        assert_eq!(httpy.url.as_deref(), Some("http://localhost:9000/mcp"));
    }

    #[test]
    fn merge_adds_skips_and_replaces_respecting_force() {
        let mut settings = Settings::default();
        settings.mcp_servers = vec![McpServerConfig {
            name: "github".to_owned(),
            disabled: false,
            transport: McpTransport::Stdio,
            command: Some("existing-binary".to_owned()),
            args: None,
            url: None,
            env: None,
            extra: Map::new(),
        }];

        let entries = parse_claude_config(CLAUDE_FIXTURE).expect("fixture");
        let outcome = merge_mcp_servers(&mut settings, entries, false);
        assert!(
            outcome.imported.contains(&"filesystem".to_owned()),
            "new name added: {outcome:?}"
        );
        assert!(
            outcome.skipped.contains(&"github".to_owned()),
            "existing name kept without force: {outcome:?}"
        );
        assert!(
            outcome.disabled.contains(&"github".to_owned()),
            "disabled flag reported: {outcome:?}"
        );
        // The existing github entry is untouched (no force).
        let github = settings
            .mcp_servers
            .iter()
            .find(|server| server.name == "github")
            .expect("github");
        assert_eq!(github.command.as_deref(), Some("existing-binary"));

        // Force replaces the existing entry.
        let entries = parse_claude_config(CLAUDE_FIXTURE).expect("fixture");
        let outcome = merge_mcp_servers(&mut settings, entries, true);
        assert!(
            outcome.replaced.contains(&"github".to_owned()),
            "force replaces: {outcome:?}"
        );
        let github = settings
            .mcp_servers
            .iter()
            .find(|server| server.name == "github")
            .expect("github");
        assert_eq!(github.command.as_deref(), Some("npx"));
        assert!(github.disabled);
        assert_eq!(settings.mcp_servers.len(), 3);
    }

    #[test]
    fn merge_rejects_invalid_entries_without_poisoning_settings() {
        let mut settings = Settings::default();
        let mut entries = parse_claude_config(CLAUDE_FIXTURE).expect("fixture");
        // An enabled stdio entry without a command is invalid.
        entries.push(McpServerConfig {
            name: "broken".to_owned(),
            disabled: false,
            transport: McpTransport::Stdio,
            command: None,
            args: None,
            url: None,
            env: None,
            extra: Map::new(),
        });
        // A disabled entry without a command is fine (it never spawns).
        entries.push(McpServerConfig {
            name: "offline".to_owned(),
            disabled: true,
            transport: McpTransport::Stdio,
            command: None,
            args: None,
            url: None,
            env: None,
            extra: Map::new(),
        });
        let outcome = merge_mcp_servers(&mut settings, entries, false);
        assert!(
            outcome
                .rejected
                .iter()
                .any(|rejected| rejected.starts_with("broken")),
            "invalid entry rejected: {outcome:?}"
        );
        assert!(outcome.imported.contains(&"offline".to_owned()));
        assert!(
            !settings
                .mcp_servers
                .iter()
                .any(|server| server.name == "broken"),
            "rejected entry must not be merged"
        );
        assert!(
            settings
                .mcp_servers
                .iter()
                .any(|server| server.name == "offline")
        );
    }

    #[test]
    fn imported_entries_round_trip_through_settings_serde() {
        // The disabled flag, env references, and extra fields all survive a
        // settings serialize/deserialize cycle.
        let mut settings = Settings::default();
        let outcome = merge_mcp_servers(
            &mut settings,
            parse_claude_config(CLAUDE_FIXTURE).expect("fixture"),
            false,
        );
        assert_eq!(outcome.imported.len() + outcome.skipped.len(), 3);

        let encoded = serde_json::to_value(&settings).expect("serialize");
        let decoded: Settings = serde_json::from_value(encoded).expect("deserialize");
        let github = decoded
            .mcp_servers
            .iter()
            .find(|server| server.name == "github")
            .expect("github");
        assert!(github.disabled, "disabled flag survives the round-trip");
        let legacy = decoded
            .mcp_servers
            .iter()
            .find(|server| server.name == "legacy")
            .expect("legacy");
        assert_eq!(
            legacy.extra.get("timeout").and_then(Value::as_i64),
            Some(10),
            "extra fields survive the round-trip"
        );
        // The whole merged settings document validates (disabled entries are
        // exempt from transport requirements).
        crate::settings::validate_settings(
            &decoded,
            SettingsScope::Global,
            Path::new("/tmp/settings.json"),
        )
        .expect("merged settings validate");
    }

    #[test]
    fn malformed_configs_fail_with_context() {
        assert!(parse_claude_config("not json").is_err(), "invalid JSON fails");
        assert!(
            parse_claude_config(r#"{"mcpServers": []}"#).is_err(),
            "mcpServers must be an object"
        );
        assert!(
            parse_claude_config(r#"{"mcpServers": {"x": "not-an-object"}}"#).is_err(),
            "entries must be objects"
        );
        assert!(
            parse_cursor_config(r#"{"mcpServers": {"x": {"type": "websocket"}}}"#).is_err(),
            "unsupported transports fail"
        );
        assert!(
            parse_cursor_config(r#"{"mcpServers": {}}"#).is_ok(),
            "an empty mcpServers object parses to no entries"
        );
    }

    #[test]
    fn claude_config_path_ends_at_the_platform_file() {
        // The resolved path is always the `claude_desktop_config.json` file
        // (directory selection is platform-specific and env-dependent).
        let path = claude_desktop_config_path();
        if let Some(path) = path {
            assert_eq!(
                path.file_name().and_then(|name| name.to_str()),
                Some("claude_desktop_config.json")
            );
        }
    }

    #[test]
    fn cursor_config_path_is_project_scoped() {
        let project = tempfile::tempdir().expect("project dir");
        assert_eq!(
            cursor_mcp_config_path(project.path()),
            project.path().join(".cursor/mcp.json")
        );
    }

    #[test]
    fn merge_outcome_empty_for_empty_input() {
        let mut settings = Settings::default();
        let outcome = merge_mcp_servers(&mut settings, Vec::new(), false);
        assert!(outcome.is_empty(), "{outcome:?}");
        assert!(settings.mcp_servers.is_empty());
    }

    #[test]
    fn imported_sse_entries_survive_validation() {
        let mut settings = Settings::default();
        let outcome = merge_mcp_servers(
            &mut settings,
            parse_cursor_config(CURSOR_FIXTURE).expect("fixture"),
            false,
        );
        assert!(
            outcome.rejected.is_empty(),
            "all cursor fixture entries are valid: {outcome:?}"
        );
        assert_eq!(settings.mcp_servers.len(), 3);
    }

    #[test]
    fn claude_entry_extra_keeps_arbitrary_fields() {
        let entry = json!({
            "command": "npx",
            "alwaysAllow": ["read"],
            "headers": { "Authorization": "Bearer x" }
        });
        let config = entry_to_config("custom", &entry).expect("parse");
        assert_eq!(
            config.extra.get("alwaysAllow").and_then(Value::as_array).map(Vec::len),
            Some(1)
        );
        assert!(config.extra.contains_key("headers"));
    }
}
