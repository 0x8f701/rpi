//! Command-line argument parsing for the `rpi` binary.
//!
//! Mirrors the Go upstream flag surface: top-level flags drive the agent run
//! path (print mode or interactive REPL), while `models`, `sessions`, and
//! `import-session` are first-class subcommands. The top-level `--export`
//! flag mirrors the `export` subcommand for upstream parity. `--version` is
//! provided by clap for release smoke tests.

use std::ffi::OsString;
use std::path::PathBuf;

use clap::{CommandFactory, Parser, Subcommand, ValueEnum};

/// `rpi` — Rust coding agent.
#[derive(Debug, Clone, Parser)]
#[command(
    name = "rpi",
    version,
    about = "rpi - Rust coding agent",
    long_about = None,
    args_override_self = true,
    propagate_version = true,
)]
pub struct Cli {
    /// Subcommand dispatch. When absent, the top-level flags drive a run.
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Export a session to a self-contained HTML file, same as the `export`
    /// subcommand (no model/auth/network required).
    #[arg(long, value_name = "SESSION_PATH")]
    pub export: Option<PathBuf>,

    /// Write the export to this explicit path (with --export).
    #[arg(long, short = 'o', value_name = "PATH", requires = "export")]
    pub output: Option<PathBuf>,

    /// Export the current branch as JSONL instead of HTML (with --export).
    #[arg(long, requires = "export")]
    pub jsonl: bool,

    /// Provider id used with --model.
    #[arg(long, value_name = "PROVIDER", requires = "model")]
    pub provider: Option<String>,

    /// Model spec (provider/id or bare id).
    #[arg(short = 'm', long, value_name = "SPEC", global = true)]
    pub model: Option<String>,

    /// Comma-separated model patterns used to scope interactive model cycling.
    #[arg(long, value_name = "PATTERNS", value_delimiter = ',')]
    pub models: Option<Vec<String>>,

    /// Print mode (non-interactive). Also selected when stdin or stdout is not a terminal.
    #[arg(short = 'p', long)]
    pub print: bool,

    /// Output protocol: text, json event stream, or JSONL rpc control.
    #[arg(long, value_enum, value_name = "MODE")]
    pub mode: Option<Mode>,

    /// Resume the most recent session for this directory.
    #[arg(short = 'c', long = "continue", conflicts_with_all = ["resume", "session", "session_id", "fork", "no_session"])]
    pub continue_latest: bool,

    /// Resume a native or foreign session by path, exact id, or unambiguous prefix.
    #[arg(short = 'r', long, value_name = "PATH_OR_ID", conflicts_with_all = ["continue_latest", "session", "session_id", "fork", "no_session"])]
    pub resume: Option<String>,


    /// Open a session by file path, exact id, or unambiguous id prefix.
    #[arg(long, value_name = "PATH_OR_ID", conflicts_with_all = ["continue_latest", "resume", "session_id", "fork", "no_session"])]
    pub session: Option<String>,

    /// Open an exact project session id, creating it when absent.
    #[arg(long, value_name = "ID", conflicts_with_all = ["continue_latest", "resume", "session", "no_session"])]
    pub session_id: Option<String>,

    /// Fork a session by file path, exact id, or unambiguous id prefix.
    #[arg(long, value_name = "PATH_OR_ID", conflicts_with_all = ["continue_latest", "resume", "session", "no_session"])]
    pub fork: Option<String>,

    /// Override the directory used for session storage and id lookup.
    #[arg(long, value_name = "DIR", global = true)]
    pub session_dir: Option<PathBuf>,

    /// Active config profile: relocates the user base dir (agent dir,
    /// sessions, settings, auth, memory, skills) to `<base>/profiles/<name>`.
    /// `default` keeps the default base; `PI_PROFILE` is honored when the flag
    /// is absent.
    #[arg(long, value_name = "NAME", global = true)]
    pub profile: Option<String>,

    /// Do not persist a session file for this run.
    #[arg(long, conflicts_with_all = ["continue_latest", "resume", "session", "session_id", "fork"])]
    pub no_session: bool,

    /// Set the session display name.
    #[arg(short = 'n', long, value_name = "NAME")]
    pub name: Option<String>,

    /// Override the system prompt with text or an existing file.
    #[arg(long = "system-prompt", alias = "system", value_name = "TEXT_OR_PATH")]
    pub system: Option<String>,

    /// Append text or an existing file to the system prompt; repeatable.
    #[arg(long = "append-system-prompt", value_name = "TEXT_OR_PATH", action = clap::ArgAction::Append)]
    pub append_system_prompt: Vec<String>,

    /// Working directory (default: current directory). Global so subcommands
    /// such as `sessions` honor `-C`/`--cwd` in either position.
    #[arg(short = 'C', long, value_name = "DIR", global = true)]
    pub cwd: Option<PathBuf>,

    /// Add a directory to scoped ls/find/grep/glob tools and @file expansion; repeatable.
    #[arg(long = "add-dir", value_name = "DIR", action = clap::ArgAction::Append)]
    pub add_dirs: Vec<PathBuf>,

    /// Reasoning level: off|minimal|low|medium|high|xhigh|max.
    #[arg(long = "thinking", alias = "think", value_name = "LEVEL")]
    pub think: Option<String>,

    /// Override the API key for the resolved model's provider (never logged).
    #[arg(long, value_name = "KEY")]
    pub api_key: Option<String>,

    /// Comma-separated allowlist applied after all tools are assembled.
    #[arg(short = 't', long, value_name = "TOOLS", value_delimiter = ',')]
    pub tools: Option<Vec<String>>,

    /// Comma-separated denylist applied after the allowlist.
    #[arg(long, value_name = "TOOLS", value_delimiter = ',')]
    pub exclude_tools: Vec<String>,

    /// Disable all built-in, extension, orchestration, and custom tools.
    #[arg(long, conflicts_with = "no_builtin_tools")]
    pub no_tools: bool,

    /// Disable built-in tools while preserving non-built-in contributions.
    #[arg(long, conflicts_with = "no_tools")]
    pub no_builtin_tools: bool,

    /// Load an explicit extension manifest or manifest directory; repeatable.
    #[arg(short = 'e', long = "extension", visible_alias = "extensions", value_name = "PATH", action = clap::ArgAction::Append)]
    pub extensions: Vec<PathBuf>,

    /// Disable discovered/configured extensions while retaining explicit --extension paths.
    #[arg(long)]
    pub no_extensions: bool,

    /// Load an explicit skill file or directory; repeatable.
    #[arg(long = "skill", value_name = "PATH", action = clap::ArgAction::Append)]
    pub skills: Vec<PathBuf>,

    /// Disable discovered/configured skills while retaining explicit --skill paths.
    #[arg(long)]
    pub no_skills: bool,

    /// Load an explicit prompt template file or directory; repeatable.
    #[arg(long = "prompt-template", value_name = "PATH", action = clap::ArgAction::Append)]
    pub prompt_templates: Vec<PathBuf>,

    /// Disable discovered/configured prompt templates while retaining explicit paths.
    #[arg(long)]
    pub no_prompt_templates: bool,

    /// Load an explicit theme file or directory; repeatable.
    #[arg(long = "theme", value_name = "PATH", action = clap::ArgAction::Append)]
    pub themes: Vec<PathBuf>,

    /// Disable discovered/configured themes while retaining explicit --theme paths.
    #[arg(long)]
    pub no_themes: bool,

    /// Disable AGENTS.md and CLAUDE.md discovery.
    #[arg(long)]
    pub no_context_files: bool,

    /// List models and exit, optionally filtering by a search string.
    #[arg(long, value_name = "SEARCH", num_args = 0..=1, default_missing_value = "")]
    pub list_models: Option<String>,

    /// Disable nonessential startup networking such as catalog refreshes and update checks.
    #[arg(long, global = true)]
    pub offline: bool,

    /// Force verbose startup diagnostics.
    #[arg(long)]
    pub verbose: bool,

    /// Trust project-local `.pi` settings/resources for this run only.
    #[arg(short = 'a', long = "approve", conflicts_with = "no_approve", global = true)]
    pub approve: bool,

    /// Refuse project-local `.pi` settings/resources for this run only.
    #[arg(long = "no-approve", conflicts_with = "approve", global = true)]
    pub no_approve: bool,

    /// Host tool approval policy: yolo, write, or ask.
    #[arg(long = "approval-mode", value_enum, value_name = "MODE", global = true)]
    pub approval_mode: Option<ApprovalModeArg>,

    /// Prompt turns. On a terminal these initialize the interactive UI; in
    /// print/structured modes each positional is sent as a separate turn.
    #[arg(value_name = "PROMPT")]
    pub prompt: Vec<String>,

    /// Run a headless Web-only HTTP/WebSocket service. This never starts the
    /// TUI or line REPL and remains alive independently of standard input.
    #[arg(long, value_name = "SOCKET_ADDR")]
    pub listen: Option<std::net::SocketAddr>,

    /// Optional bearer token file for --listen. When set, /ws and /rpc
    /// require the token from this file (as `Authorization: Bearer <token>`
    /// or the `rpi-auth.<token>` subprotocol); without it the listener is
    /// tokenless and accepts browsers directly.
    #[arg(long, value_name = "PATH", requires = "listen")]
    pub listen_token_file: Option<PathBuf>,

    /// Explicitly allow plaintext HTTP/WebSocket on a non-loopback --listen
    /// address. A token file is optional (strongly recommended): passive
    /// network observers can capture control traffic and, when configured,
    /// the bearer token.
    #[arg(long, requires = "listen")]
    pub listen_allow_insecure_remote: bool,

    /// Advertised HTTP(S) origin used to build collaboration links when
    /// --listen binds a wildcard address (0.0.0.0 or ::). Strict origin:
    /// http/https scheme, a host with an optional numeric port, and no
    /// credentials, path, query, or fragment (a trailing "/" is normalized
    /// away). Loopback binds advertise their local address automatically;
    /// wildcard binds fail closed without this flag.
    #[arg(long, value_name = "URL", requires = "listen")]
    pub listen_advertised_origin: Option<String>,

    /// Explicitly disable TLS on --listen and serve plaintext HTTP/WebSocket
    /// instead. Without this flag the listener terminates HTTPS: with a real
    /// certificate pair (--listen-cert/--listen-key) or with a self-signed
    /// certificate auto-generated and cached under ~/.pi/agent/. Plaintext
    /// exposes control traffic to network observers; use it only on a
    /// trusted network, ideally with --listen-token-file.
    #[arg(long, requires = "listen", conflicts_with_all = ["listen_cert", "listen_key"])]
    pub listen_plaintext: bool,

    /// TLS certificate file (PEM) for HTTPS. When paired with --listen-key,
    /// the listener uses TLS. When both are absent, a self-signed cert is
    /// auto-generated and cached under ~/.pi/agent/.
    #[arg(long, value_name = "PATH", requires_all = ["listen", "listen_key"])]
    pub listen_cert: Option<PathBuf>,

    /// TLS private key file (PEM) for HTTPS. Must be paired with --listen-cert.
    #[arg(long, value_name = "PATH", requires_all = ["listen", "listen_cert"])]
    pub listen_key: Option<PathBuf>,
}

/// Headless application adapters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Mode {
    Text,
    Json,
    Rpc,
}

/// Maximum length of a config profile name.
pub const MAX_PROFILE_NAME_LENGTH: usize = 64;

/// Validate a config profile name: 1-64 ASCII letters, digits, `-`, or `_`
/// (whitespace is trimmed first; `default` is a valid name that selects the
/// default profile). Returns an actionable message on failure.
pub fn validate_profile_name(name: &str) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("profile name cannot be empty".to_owned());
    }
    if name.chars().count() > MAX_PROFILE_NAME_LENGTH {
        return Err(format!(
            "profile name {name:?} exceeds {MAX_PROFILE_NAME_LENGTH} characters"
        ));
    }
    if !name
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || character == '-' || character == '_')
    {
        return Err(format!(
            "invalid profile name {name:?}: use only letters, digits, '-' and '_' (at most {MAX_PROFILE_NAME_LENGTH} characters)"
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ApprovalModeArg {
    Yolo,
    Write,
    Ask,
}

impl From<ApprovalModeArg> for pi_agent::ApprovalMode {
    fn from(value: ApprovalModeArg) -> Self {
        match value {
            ApprovalModeArg::Yolo => Self::Yolo,
            ApprovalModeArg::Write => Self::Write,
            ApprovalModeArg::Ask => Self::Ask,
        }
    }
}

/// Shell supported by the `completion` subcommand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CompletionShell {
    /// Bourne-again shell completions.
    Bash,
    /// Z shell completions.
    Zsh,
    /// Friendly interactive shell completions.
    Fish,
}

impl CompletionShell {
    /// Map to the underlying `clap_complete` shell.
    #[must_use]
    pub fn to_clap_shell(self) -> clap_complete::Shell {
        match self {
            Self::Bash => clap_complete::Shell::Bash,
            Self::Zsh => clap_complete::Shell::Zsh,
            Self::Fish => clap_complete::Shell::Fish,
        }
    }
}

/// Generate shell completions for the `rpi` command into `writer`.
pub fn write_completion<W: std::io::Write>(shell: CompletionShell, writer: &mut W) {
    let mut cmd = Cli::command();
    clap_complete::generate(shell.to_clap_shell(), &mut cmd, "rpi", writer);
}

/// First-class subcommands.
#[derive(Debug, Clone, Subcommand)]
pub enum Command {
    /// Configure an API key or provider subscription.
    Login {
        /// Provider id; omit in an interactive terminal to choose from a list.
        #[arg(value_name = "PROVIDER")]
        provider: Option<String>,
        /// Store the credential under this scope label (e.g. "work"). The
        /// active scope (`PI_AUTH_SCOPE` or the authScope setting) then picks
        /// this credential over the unscoped default.
        #[arg(long, value_name = "LABEL")]
        scope: Option<String>,
    },
    /// Remove the stored credential for one provider.
    Logout {
        /// Provider id; omit in an interactive terminal to choose from configured providers.
        #[arg(value_name = "PROVIDER")]
        provider: Option<String>,
        /// Remove the credential stored under this scope label instead of the
        /// unscoped default.
        #[arg(long, value_name = "LABEL")]
        scope: Option<String>,
    },
    /// List available models, optionally filtered by a substring.
    Models {
        /// Substring matched against provider or model id.
        #[arg(value_name = "FILTER")]
        filter: Option<String>,
    },
    /// List saved sessions for this directory.
    Sessions,
    /// Import an external agent session into native Pi v3 JSONL.
    ImportSession {
        /// Source format: pi|omp|codex|claude|grok|droid.
        #[arg(value_name = "SOURCE")]
        source: String,
        /// Input session file path (or source id for codex).
        #[arg(value_name = "INPUT")]
        input: PathBuf,
        /// Write the emitted session to this explicit path (must not exist).
        #[arg(long, value_name = "PATH")]
        output: Option<PathBuf>,
    },
    /// Validate and print the active settings/resource snapshot.
    Reload,
    /// Diagnose the rpi environment and report PASS/FAIL per check. Never
    /// prints secret material: auth presence is reported as provider names
    /// and the file path only, never credential contents.
    Doctor {
        /// Emit a machine-readable JSON report instead of plain text.
        #[arg(long)]
        json: bool,
    },
    /// Print what to configure and where: the models.json and auth.json
    /// paths, with example contents on an interactive terminal.
    Setup {
        /// Emit a machine-readable JSON report instead of plain text.
        #[arg(long)]
        json: bool,
    },
    /// Summarize session and usage state for this directory: session count,
    /// latest session, goal state (if any), and available tools.
    Dashboard {
        /// Emit a machine-readable JSON report instead of plain text.
        #[arg(long)]
        json: bool,
    },
    /// Export a session to a self-contained HTML file (no model/auth/network).
    Export {
        /// Session file to export.
        #[arg(value_name = "SESSION_PATH")]
        session: PathBuf,
        /// Write the HTML to this explicit path.
        #[arg(long, short = 'o', value_name = "PATH")]
        output: Option<PathBuf>,
        /// Export the current branch as JSONL instead of HTML.
        #[arg(long)]
        jsonl: bool,
    },
    /// Install a local directory or git package.
    Install {
        /// Local path or git package source.
        #[arg(value_name = "SOURCE")]
        source: String,
        /// Persist the package in project-local settings instead of global settings.
        #[arg(long)]
        local: bool,
    },
    /// Remove a configured package.
    #[command(alias = "uninstall")]
    Remove {
        /// Local path or git package source.
        #[arg(value_name = "SOURCE")]
        source: String,
        /// Remove the package from project-local settings instead of global settings.
        #[arg(long)]
        local: bool,
    },
    /// List configured packages.
    List,
    /// Configure enabled package resources for global or project scope, or
    /// inspect/change settings keys (`rpi config get|set|reset|list`).
    Config {
        /// Edit project-local settings instead of global settings.
        #[arg(short = 'l', long)]
        local: bool,
        #[command(subcommand)]
        command: Option<ConfigCommand>,
    },
    /// Update rpi itself, configured extensions, or model catalogs.
    Update {
        /// Explicitly update the managed rpi installation (also the default with no target).
        #[arg(long = "self", conflicts_with_all = ["package", "extension", "models", "all"])]
        self_update: bool,
        /// Update every configured extension; combine with --self to update both.
        #[arg(long, conflicts_with_all = ["extension", "models", "all"])]
        extensions: bool,
        /// Update rpi and every configured extension.
        #[arg(long, conflicts_with_all = ["self_update", "extensions", "package", "extension", "models"])]
        all: bool,
        /// Refresh dynamic model catalogs.
        #[arg(long, conflicts_with_all = ["self_update", "extensions", "all", "package", "extension", "force"])]
        models: bool,
        /// Update one configured extension by source identity.
        #[arg(long, value_name = "SOURCE", conflicts_with_all = ["self_update", "extensions", "all", "models", "package"])]
        extension: Option<String>,
        /// Reinstall the selected self-update even when version and checksum match.
        #[arg(long, conflicts_with_all = ["models", "extension"])]
        force: bool,
        /// Update one configured extension by source identity; `self` and `rpi` select rpi itself.
        #[arg(value_name = "PACKAGE", conflicts_with_all = ["all", "models", "extension"])]
        package: Option<String>,
    },
    /// Manage a configured llama.cpp router and local GGUF downloads.
    Llama {
        #[command(subcommand)]
        command: LlamaCommand,
    },
    /// Manage marketplace plugins (packaged extensions): list, install,
    /// remove, and update. Installed plugins land in
    /// `<agent_dir>/extensions/<name>/` and are loadable as extensions once
    /// trusted by the trust store.
    Plugin {
        #[command(subcommand)]
        command: PluginCommand,
    },
    /// Serve the Agent Client Protocol (agentclientprotocol.com) so ACP-speaking
    /// editors can embed rpi as a coding agent. `stdio` speaks JSON-RPC 2.0
    /// over stdin/stdout with Content-Length framing; `serve` speaks it over a
    /// local WebSocket endpoint.
    Agent {
        #[command(subcommand)]
        command: AgentCommand,
    },
    /// Manage Model Context Protocol (MCP) servers: list configured servers
    /// and import definitions from Claude Desktop or Cursor config files.
    Mcp {
        #[command(subcommand)]
        command: McpCommand,
    },
    /// rpi headless RPC server: serve the JSONL RPC control plane on
    /// stdin/stdout (same as `--mode rpc`).
    #[command(display_name = "rpi rpc")]
    Rpc,
    /// Generate shell completions for bash, zsh, or fish.
    Completion {
        /// Target shell.
        #[arg(value_name = "SHELL")]
        shell: CompletionShell,
    },
}

/// Settings-key operations for `rpi config` (OMP `omp config get/set/reset/list`
/// parity). `rpi config` with no subcommand keeps the package-resource
/// selector; these verbs reuse the settings catalog and the atomic draft+apply
/// pipeline, so scripts can configure rpi without the TUI.
#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum ConfigCommand {
    /// Print one setting's effective value, source, and behavior.
    Get {
        /// Catalog setting key (e.g. `retry.maxRetries`).
        #[arg(value_name = "KEY")]
        key: String,
        /// Settings layer to read: global (default) or project.
        #[arg(long, value_enum)]
        scope: Option<ConfigScopeArg>,
        /// Emit machine-readable JSON instead of plain text.
        #[arg(long)]
        json: bool,
    },
    /// Set a setting through the atomic draft+apply path (identical
    /// validation and scope rules to the TUI).
    Set {
        /// Catalog setting key (e.g. `retry.maxRetries`).
        #[arg(value_name = "KEY")]
        key: String,
        /// Typed value: booleans/integers/enums pass through directly, arrays
        /// and objects are parsed as JSON (e.g. `[{"source":"example"}]`).
        #[arg(value_name = "VALUE")]
        value: String,
        /// Settings layer to write: global (default) or project.
        #[arg(long, value_enum)]
        scope: Option<ConfigScopeArg>,
        /// Emit machine-readable JSON instead of plain text.
        #[arg(long)]
        json: bool,
    },
    /// Reset a setting to its default/inherited value.
    Reset {
        /// Catalog setting key (e.g. `retry.maxRetries`).
        #[arg(value_name = "KEY")]
        key: String,
        /// Settings layer to clear: global (default) or project.
        #[arg(long, value_enum)]
        scope: Option<ConfigScopeArg>,
        /// Emit machine-readable JSON instead of plain text.
        #[arg(long)]
        json: bool,
    },
    /// List every catalog setting, grouped by category.
    List {
        /// Restrict output to one category (Models, Session, Compaction,
        /// RetryTransport, TerminalUi, Orchestration, Resources,
        /// TrustSecurity, Live).
        #[arg(long, value_name = "CATEGORY")]
        category: Option<String>,
        /// Settings layer to report: global (default) or project.
        #[arg(long, value_enum)]
        scope: Option<ConfigScopeArg>,
        /// Emit machine-readable JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
}

/// Settings scope selector for `rpi config get/set/reset/list`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ConfigScopeArg {
    Global,
    Project,
}

/// MCP server management commands.
#[derive(Debug, Clone, Subcommand)]
pub enum McpCommand {
    /// List configured MCP servers from settings (name, transport,
    /// command/url, disabled state). Entries with `disabled: true` are never
    /// spawned by the session and are marked here.
    List {
        /// Read project-local settings instead of global settings.
        #[arg(short = 'l', long)]
        local: bool,
    },
    /// Import MCP servers from a Claude Desktop or Cursor config file into
    /// settings. Entries are validated individually; existing servers are
    /// never overwritten unless `--force` is given.
    Import {
        /// Config format: claude|cursor|auto (default: auto — try Claude
        /// Desktop, then Cursor in the current project).
        #[arg(long, value_name = "SOURCE", value_enum)]
        source: Option<McpImportSourceArg>,
        /// Read from this explicit config file instead of the standard
        /// location. With `--source auto`, a file named `mcp.json` parses as
        /// Cursor, anything else as Claude Desktop.
        #[arg(long, value_name = "PATH")]
        file: Option<PathBuf>,
        /// Write into project-local settings instead of global settings.
        #[arg(short = 'l', long)]
        local: bool,
        /// Overwrite existing servers with the same name instead of skipping them.
        #[arg(long)]
        force: bool,
    },
}

/// Config format selector for `rpi mcp import`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum McpImportSourceArg {
    /// Claude Desktop's `claude_desktop_config.json`.
    Claude,
    /// Cursor's `.cursor/mcp.json`.
    Cursor,
    /// Try Claude Desktop first, then the project's Cursor config.
    Auto,
}

/// Agent Client Protocol transports.
#[derive(Debug, Clone, Subcommand)]
pub enum AgentCommand {
    /// ACP over stdio: the client launches `rpi agent stdio` as a subprocess
    /// and exchanges Content-Length framed JSON-RPC messages on stdin/stdout.
    Stdio,
    /// ACP over a local WebSocket server. Each connected client speaks plain
    /// JSON-RPC 2.0 messages as WebSocket text frames (no Content-Length
    /// headers — the WebSocket frame replaces them).
    Serve {
        /// Loopback socket address to bind (plaintext; non-loopback is refused
        /// until TLS support lands).
        #[arg(long, value_name = "ADDR", default_value = "127.0.0.1:34567")]
        address: std::net::SocketAddr,
        /// Optional token file for WebSocket connections. The server is
        /// loopback-only (plaintext WebSocket cannot safely carry the token
        /// off the local host); on loopback this switches the server from
        /// native-client-only to token-gated. Clients present the token as
        /// `Authorization: Bearer <token>` or the `rpi-auth.<token>`
        /// subprotocol.
        #[arg(long, value_name = "FILE")]
        token_file: Option<PathBuf>,
    },
}

/// Plugin marketplace commands.
#[derive(Debug, Clone, Subcommand)]
pub enum PluginCommand {
    /// List installed plugins with name, version, runtime, and trust state.
    List {
        /// Check the marketplace index and print available updates.
        #[arg(long)]
        updates: bool,
    },
    /// Install a plugin from a local directory, a local or remote
    /// .tgz/.tar.gz/.tar archive, an owner/repo GitHub reference, an
    /// npm:<name>[@<version>] reference, or a git URL (git+https://host/owner/
    /// repo, git+ssh://git@host/owner/repo.git, https://host/owner/repo.git,
    /// ssh://git@host/owner/repo.git, git@host:owner/repo.git).
    Install {
        #[arg(value_name = "SOURCE")]
        source: String,
    },
    /// Remove an installed plugin.
    Remove {
        #[arg(value_name = "NAME")]
        name: String,
    },
    /// Update one installed plugin from the marketplace index.
    Update {
        #[arg(value_name = "NAME")]
        name: String,
    },
}

/// llama.cpp router and GGUF management commands.
#[derive(Debug, Clone, Subcommand)]
pub enum LlamaCommand {
    /// Configure and validate a llama.cpp router.
    Configure {
        /// Router base URL (without /v1).
        #[arg(value_name = "URL")]
        base_url: String,
        /// Optional router bearer token.
        #[arg(long, value_name = "KEY")]
        api_key: Option<String>,
    },
    /// Show the router management catalog.
    Status {
        /// Ask the router to rescan its model directory.
        #[arg(long)]
        reload: bool,
    },
    /// Refresh live inference models, falling back to the persisted catalog.
    Refresh,
    /// Explicitly load a model through the router.
    Load {
        #[arg(value_name = "MODEL")]
        model: String,
    },
    /// Explicitly unload a model through the router.
    Unload {
        #[arg(value_name = "MODEL")]
        model: String,
    },
    /// Search Hugging Face for GGUF repositories.
    Search {
        #[arg(value_name = "QUERY")]
        query: String,
    },
    /// List GGUF quantizations and checksums for a repository.
    Details {
        #[arg(value_name = "OWNER/REPOSITORY")]
        repository: String,
    },
    /// Download one GGUF quantization atomically with resume support.
    Download {
        #[arg(value_name = "OWNER/REPOSITORY")]
        repository: String,
        /// Quantization name, e.g. Q4_K_M (defaults to recommended/first).
        #[arg(long, short = 'q', value_name = "QUANTIZATION")]
        quantization: Option<String>,
    },
    /// List locally installed GGUF downloads.
    Installed,
}

impl Cli {
    /// Parse process arguments after normalizing upstream's multi-character
    /// short aliases, which clap intentionally does not model as short flags.
    pub fn parse() -> Self {
        match Self::try_parse_from(std::env::args_os()) {
            Ok(cli) => cli,
            Err(error) => error.exit(),
        }
    }

    /// Fallible parser with upstream-compatible multi-character short aliases.
    pub fn try_parse_from<I, T>(arguments: I) -> std::result::Result<Self, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString> + Clone,
    {
        let normalized = normalize_short_aliases(arguments);
        <Self as Parser>::try_parse_from(normalized)
    }

    /// Validate combinations whose constraints require value inspection.
    pub fn validate(&self) -> Result<(), String> {
        if self.provider.is_some() && self.model.is_none() {
            return Err("--provider requires --model".to_owned());
        }
        if self.api_key.is_some() && self.model.is_none() && self.models.is_none() {
            return Err("--api-key requires --model or --models".to_owned());
        }
        if self
            .name
            .as_deref()
            .is_some_and(|name| name.trim().is_empty())
        {
            return Err("--name requires a non-empty value".to_owned());
        }
        if self
            .session_id
            .as_deref()
            .is_some_and(|id| id.trim().is_empty())
        {
            return Err("--session-id requires a non-empty value".to_owned());
        }
        if let Some(profile) = self.profile.as_deref() {
            let name = profile.trim();
            if !name.is_empty() && name != "default" {
                validate_profile_name(name)
                    .map_err(|message| format!("--profile: {message}"))?;
            }
        }
        if self
            .models
            .as_ref()
            .is_some_and(|patterns| patterns.iter().any(|pattern| pattern.is_empty()))
        {
            return Err("--models contains an empty pattern".to_owned());
        }
        if let Some(Command::Update {
            self_update,
            extensions,
            all,
            force,
            package,
            ..
        }) = &self.command
        {
            let positional_self = package
                .as_deref()
                .is_some_and(|source| matches!(source, "self" | "rpi"));
            if package.is_some()
                && !positional_self
                && (*self_update || *extensions || *force)
            {
                return Err(
                    "a package source cannot be combined with --self, --extensions, or --force"
                        .to_owned(),
                );
            }
            let default_self = !*all && !*self_update && !*extensions && package.is_none();
            let includes_self = *all || *self_update || positional_self || default_self;
            if *force && !includes_self {
                return Err("--force only applies to rpi self-update targets".to_owned());
            }
        }
        if self.listen.is_some() && self.command.is_some() {
            return Err("--listen cannot be combined with subcommands".to_owned());
        }
        if self.listen.is_some() && self.export.is_some() {
            return Err("--listen cannot be combined with --export".to_owned());
        }
        if self.export.is_some() && self.command.is_some() {
            return Err("--export cannot be combined with subcommands".to_owned());
        }
        // `rpi rpc` ≡ `rpi --mode rpc`: the subcommand forces RPC mode, so an
        // explicit conflicting `--mode` is rejected rather than silently
        // overridden (mirrors the old rpi-rpc wrapper's args_override_self
        // authority with explicit rejection).
        if matches!(self.command, Some(Command::Rpc))
            && matches!(self.mode, Some(mode) if mode != Mode::Rpc)
        {
            return Err(
                "`rpc` subcommand forces RPC mode and conflicts with --mode (use `rpi rpc` or `rpi --mode rpc`, not both)"
                    .to_owned(),
            );
        }
        if self.listen.is_some()
            && matches!(self.mode, Some(Mode::Json) | Some(Mode::Rpc))
        {
            return Err("--listen selects the Web-only service and cannot be combined with JSON/RPC modes".to_owned());
        }
        if self.listen.is_some() && self.is_print_mode() {
            return Err("--listen selects the Web-only service and cannot be combined with print mode".to_owned());
        }
        if self.listen.is_some() && !self.prompt.is_empty() {
            return Err("--listen is Web-only and cannot be combined with positional prompts; submit prompts through /web, /ws, or /rpc".to_owned());
        }
        if self.listen.is_some() && self.list_models.is_some() {
            return Err("--listen cannot be combined with --list-models".to_owned());
        }
        if let Some(path) = self.listen_token_file.as_deref()
            && self.listen.is_none()
        {
            return Err("--listen-token-file requires --listen".to_owned());
        }
        if let Some(path) = self.listen_token_file.as_deref()
            && path.as_os_str().is_empty()
        {
            return Err("--listen-token-file requires a non-empty path".to_owned());
        }
        if self.listen_allow_insecure_remote && self.listen.is_none() {
            return Err("--listen-allow-insecure-remote requires --listen".to_owned());
        }
        if let Some(origin) = self.listen_advertised_origin.as_deref()
            && let Err(error) = crate::modes::listen::parse_advertised_origin(origin)
        {
            return Err(error.to_string());
        }
        for (flag, names) in [
            ("--tools", self.tools.as_deref()),
            ("--exclude-tools", Some(self.exclude_tools.as_slice())),
        ] {
            if names.is_some_and(|names| names.iter().any(|name| name.trim().is_empty())) {
                return Err(format!("{flag} contains an empty tool name"));
            }
        }
        Ok(())
    }

    /// Whether print mode was explicitly requested with `-p` / `--print`.
    ///
    /// Positional messages are initial messages, not an implicit print-mode
    /// switch. Terminal detection selects interactive versus print mode.
    #[must_use]
    pub fn is_print_mode(&self) -> bool {
        self.print
    }

    /// The joined prompt argument.
    #[must_use]
    pub fn prompt_text(&self) -> String {
        self.prompt.join(" ")
    }
}

fn normalize_short_aliases<I, T>(arguments: I) -> Vec<OsString>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    arguments
        .into_iter()
        .map(Into::into)
        .map(|argument| match argument.to_str() {
            Some("-v") => OsString::from("--version"),
            Some("-xt") => OsString::from("--exclude-tools"),
            Some("-nt") => OsString::from("--no-tools"),
            Some("-nbt") => OsString::from("--no-builtin-tools"),
            Some("-ne") => OsString::from("--no-extensions"),
            Some("-ns") => OsString::from("--no-skills"),
            Some("-np") => OsString::from("--no-prompt-templates"),
            Some("-nc") => OsString::from("--no-context-files"),
            Some("-na") => OsString::from("--no-approve"),
            _ => argument,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn positional_prompts_remain_interactive_without_print() {
        let empty = Cli::try_parse_from(["rpi", ""]).expect("parse empty positional");
        assert!(empty.prompt_text().is_empty());
        assert!(!empty.is_print_mode());
        let cli = Cli::try_parse_from(["rpi", "hello", "world"]).expect("parse prompts");
        assert_eq!(cli.prompt, ["hello", "world"]);
        assert_eq!(cli.prompt_text(), "hello world");
        assert!(!cli.is_print_mode());
    }

    #[test]
    fn explicit_print_flag_forces_print_mode() {
        let cli = Cli::try_parse_from(["rpi", "-p"]).expect("parse -p");
        assert!(cli.is_print_mode());
        let cli = Cli::try_parse_from(["rpi", "--print", ""]).expect("parse --print empty");
        assert!(cli.is_print_mode());
    }

    #[test]
    fn parses_core_parity_flags_and_repeats() {
        let cli = Cli::try_parse_from([
            "rpi", "--provider", "openai", "--model", "gpt-5", "--models",
            "openai/*,*sonnet*:high", "--system-prompt", "system.txt",
            "--append-system-prompt", "one", "--append-system-prompt", "two",
            "--name", "named", "--session-id", "session_1", "--session-dir",
            "sessions", "--thinking", "high", "--mode", "text", "--offline",
            "--verbose", "-t", "read,custom", "-xt", "bash,task", "-e", "one-ext",
            "--extension", "two-ext", "--skill", "skill-a", "--skill", "skill-b",
            "--prompt-template", "prompt-a", "--prompt-template", "prompt-b",
            "--theme", "theme-a", "--theme", "theme-b", "--add-dir", "workspace-a",

            "--add-dir", "workspace-b",
        ])
        .expect("parse parity flags");
        assert_eq!(cli.provider.as_deref(), Some("openai"));
        assert_eq!(cli.model.as_deref(), Some("gpt-5"));
        assert_eq!(cli.models.as_ref().map(Vec::len), Some(2));
        assert_eq!(cli.append_system_prompt, ["one", "two"]);
        assert_eq!(cli.name.as_deref(), Some("named"));
        assert_eq!(cli.session_id.as_deref(), Some("session_1"));
        assert_eq!(cli.session_dir.as_deref(), Some(PathBuf::from("sessions").as_path()));
        assert_eq!(cli.think.as_deref(), Some("high"));
        assert_eq!(cli.mode, Some(Mode::Text));
        assert_eq!(cli.tools.as_ref().map(Vec::len), Some(2));
        assert_eq!(cli.exclude_tools, ["bash", "task"]);
        assert_eq!(cli.extensions.len(), 2);
        assert_eq!(cli.skills.len(), 2);
        assert_eq!(cli.prompt_templates.len(), 2);
        assert_eq!(cli.themes.len(), 2);
        assert_eq!(cli.add_dirs, [PathBuf::from("workspace-a"), PathBuf::from("workspace-b")]);
        assert!(cli.offline && cli.verbose);
    }
    #[test]
    fn parses_approval_mode_and_rejects_invalid_values() {
        for (wire, expected) in [
            ("yolo", ApprovalModeArg::Yolo),
            ("write", ApprovalModeArg::Write),
            ("ask", ApprovalModeArg::Ask),
        ] {
            let cli = Cli::try_parse_from(["rpi", "--approval-mode", wire]).expect("approval mode");
            assert_eq!(cli.approval_mode, Some(expected));
        }
        assert!(Cli::try_parse_from(["rpi", "--approval-mode", "WRITE"]).is_err());
        assert!(Cli::try_parse_from(["rpi", "--approval-mode", "always"]).is_err());
    }

    #[test]
    fn parses_disable_aliases_session_paths_and_optional_model_search() {
        let cli = Cli::try_parse_from([
            "rpi", "-ne", "-ns", "-np", "--no-themes", "-nc", "-nbt",
            "--session", "sessions/a.jsonl",
        ]).expect("disable aliases");
        assert!(cli.no_extensions && cli.no_skills && cli.no_prompt_templates);
        assert!(cli.no_themes && cli.no_context_files && cli.no_builtin_tools);
        assert_eq!(cli.session.as_deref(), Some("sessions/a.jsonl"));
        let all = Cli::try_parse_from(["rpi", "--list-models"]).expect("list all");
        assert_eq!(all.list_models.as_deref(), Some(""));
        let searched = Cli::try_parse_from(["rpi", "--list-models", "sonnet"])
            .expect("list search");
        assert_eq!(searched.list_models.as_deref(), Some("sonnet"));
    }

    #[test]
    fn parses_unified_resume_and_rejects_removed_codex_flag() {
        let unified = Cli::try_parse_from(["rpi", "--resume", "grok-prefix"])
            .expect("unified resume");
        assert_eq!(unified.resume.as_deref(), Some("grok-prefix"));
        assert!(Cli::try_parse_from(["rpi", "--resume-codex", "codex-id"]).is_err());
    }
    #[test]
    fn resume_short_alias_sets_resume_and_retains_conflicts() {
        let cli = Cli::try_parse_from(["rpi", "-r", "session-id"])
            .expect("short resume alias");
        assert_eq!(cli.resume.as_deref(), Some("session-id"));
        // The short alias participates in the same selector conflicts as --resume.
        assert!(Cli::try_parse_from(["rpi", "-r", "id", "--session", "other"]).is_err());
        assert!(Cli::try_parse_from(["rpi", "-r", "id", "--continue"]).is_err());
        assert!(Cli::try_parse_from(["rpi", "-r", "id", "--no-session"]).is_err());
    }

    #[test]
    fn rejects_conflicts_empty_values_and_unknown_flags() {
        for args in [
            ["rpi", "--session", "id", "--fork", "other"].as_slice(),
            ["rpi", "--no-session", "--continue"].as_slice(),
            ["rpi", "--no-tools", "--no-builtin-tools"].as_slice(),
            ["rpi", "--approve", "--no-approve"].as_slice(),
        ] {
            assert!(Cli::try_parse_from(args).is_err(), "accepted conflict: {args:?}");
        }
        for args in [
            ["rpi", "--name", ""].as_slice(),
            ["rpi", "--session-id", ""].as_slice(),
            ["rpi", "--models", "a,,b"].as_slice(),
            ["rpi", "--tools", "read,,bash"].as_slice(),
        ] {
            let cli = Cli::try_parse_from(args).expect("parse for validation");
            assert!(cli.validate().is_err(), "accepted empty value: {args:?}");
        }
        assert!(Cli::try_parse_from(["rpi", "--provider", "openai"]).is_err());
        assert!(Cli::try_parse_from(["rpi", "--unknown-extension-flag"]).is_err());
    }
    #[test]
    fn parses_package_aliases_update_targets_and_global_trust_flags() {
        let uninstall = Cli::try_parse_from(["rpi", "uninstall", "package", "--approve"])
            .expect("uninstall alias and trailing trust flag");
        assert!(uninstall.approve);
        assert!(matches!(uninstall.command, Some(Command::Remove { ref source, .. }) if source == "package"));

        let no_approve = Cli::try_parse_from(["rpi", "config", "-na"])
            .expect("no-approve alias after command");
        assert!(no_approve.no_approve);

        // `rpi config` settings-key verbs (OMP `omp config` parity).
        let get = Cli::try_parse_from(["rpi", "config", "get", "retry.maxRetries", "--scope", "project", "--json"])
            .expect("config get");
        assert!(matches!(
            get.command,
            Some(Command::Config {
                local: false,
                command: Some(ConfigCommand::Get { ref key, scope: Some(ConfigScopeArg::Project), json: true, .. }),
            }) if key == "retry.maxRetries"
        ));
        let set = Cli::try_parse_from(["rpi", "config", "set", "transport", "sse"])
            .expect("config set");
        assert!(matches!(
            set.command,
            Some(Command::Config {
                local: false,
                command: Some(ConfigCommand::Set { ref key, ref value, scope: None, json: false, .. }),
            }) if key == "transport" && value == "sse"
        ));
        let reset = Cli::try_parse_from(["rpi", "config", "--local", "reset", "theme"])
            .expect("config reset with --local");
        assert!(matches!(
            reset.command,
            Some(Command::Config {
                local: true,
                command: Some(ConfigCommand::Reset { ref key, .. }),
            }) if key == "theme"
        ));
        let list = Cli::try_parse_from(["rpi", "config", "list", "--category", "Models"])
            .expect("config list");
        assert!(matches!(
            list.command,
            Some(Command::Config {
                local: false,
                command: Some(ConfigCommand::List { ref category, json: false, .. }),
            }) if category.as_deref() == Some("Models")
        ));
        // Bare `rpi config` keeps the package-resource selector.
        let bare = Cli::try_parse_from(["rpi", "config"]).expect("bare config");
        assert!(matches!(
            bare.command,
            Some(Command::Config { local: false, command: None })
        ));

        for args in [
            ["rpi", "update", "--all"].as_slice(),
            ["rpi", "update", "--models"].as_slice(),
            ["rpi", "update", "--extension", "package"].as_slice(),
            ["rpi", "update", "self"].as_slice(),
            ["rpi", "update", "rpi", "--extensions"].as_slice(),
        ] {
            let cli = Cli::try_parse_from(args).expect("parse update target");
            cli.validate().expect("validate update target");
        }
    }


    #[test]
    fn validates_update_target_combinations() {
        let all = Cli::try_parse_from(["rpi", "update", "--all", "--force"])
            .expect("parse all force");
        all.validate().expect("all includes self");

        let alias = Cli::try_parse_from([
            "rpi",
            "update",
            "self",
            "--extensions",
            "--force",
        ])
        .expect("parse self alias");
        alias.validate().expect("self alias includes self");

        let extensions = Cli::try_parse_from(["rpi", "update", "--extensions", "--force"])
            .expect("parse extensions force");
        assert!(extensions.validate().is_err());

        let package = Cli::try_parse_from(["rpi", "update", "git:example/repo", "--extensions"])
            .expect("parse package extensions");
        assert!(package.validate().is_err());
    }
    #[test]
    fn parses_plugin_subcommands() {
        let list = Cli::try_parse_from(["rpi", "plugin", "list"]).expect("plugin list");
        assert!(matches!(
            list.command,
            Some(Command::Plugin {
                command: PluginCommand::List { updates: false }
            })
        ));

        let updates =
            Cli::try_parse_from(["rpi", "plugin", "list", "--updates"]).expect("plugin list --updates");
        assert!(matches!(
            updates.command,
            Some(Command::Plugin {
                command: PluginCommand::List { updates: true }
            })
        ));

        let install = Cli::try_parse_from(["rpi", "plugin", "install", "./ext"])
            .expect("plugin install");
        assert!(matches!(
            install.command,
            Some(Command::Plugin {
                command: PluginCommand::Install { .. }
            })
        ));

        let remove = Cli::try_parse_from(["rpi", "plugin", "remove", "ext"])
            .expect("plugin remove");
        assert!(matches!(
            remove.command,
            Some(Command::Plugin {
                command: PluginCommand::Remove { .. }
            })
        ));

        let update = Cli::try_parse_from(["rpi", "plugin", "update", "ext"])
            .expect("plugin update");
        assert!(matches!(
            update.command,
            Some(Command::Plugin {
                command: PluginCommand::Update { .. }
            })
        ));

        // `rpi plugin update` must not be confused with the top-level `rpi update`.
        let top_level = Cli::try_parse_from(["rpi", "update", "self"]).expect("rpi update self");
        assert!(matches!(top_level.command, Some(Command::Update { .. })));

        // `rpi plugin` without a subcommand is a parse error.
        assert!(Cli::try_parse_from(["rpi", "plugin"]).is_err());
    }

    #[test]
    fn rpc_subcommand_parses_and_rejects_conflicting_mode() {
        // `rpi rpc` selects the subcommand with no explicit mode; dispatch
        // resolves it to forced RPC mode (≡ `rpi --mode rpc`).
        let rpc = Cli::try_parse_from(["rpi", "rpc"]).expect("parse rpi rpc");
        assert!(matches!(rpc.command, Some(Command::Rpc)));
        assert_eq!(rpc.mode, None, "rpc subcommand must not set --mode itself");
        rpc.validate().expect("rpi rpc validates");

        // `--mode rpc` before the subcommand is redundant but consistent: allowed.
        let redundant =
            Cli::try_parse_from(["rpi", "--mode", "rpc", "rpc"]).expect("parse redundant mode");
        redundant.validate().expect("--mode rpc matches the rpc subcommand");

        // A conflicting explicit --mode is rejected: after the subcommand it
        // is an unexpected top-level argument (--mode is not global)...
        assert!(
            Cli::try_parse_from(["rpi", "rpc", "--mode", "json"]).is_err(),
            "rpi rpc --mode json must be a parse error"
        );
        // ...and before the subcommand it parses but fails validation.
        let conflicting =
            Cli::try_parse_from(["rpi", "--mode", "json", "rpc"]).expect("parses conflicting");
        let error = conflicting
            .validate()
            .expect_err("conflicting --mode must fail validation");
        assert!(
            error.contains("forces RPC mode"),
            "conflict error must be actionable: {error}"
        );

        // Back-compat: `rpi --mode rpc` still parses and validates unchanged.
        let back_compat = Cli::try_parse_from(["rpi", "--mode", "rpc"]).expect("parse --mode rpc");
        assert!(matches!(back_compat.command, None));
        assert_eq!(back_compat.mode, Some(Mode::Rpc));
        back_compat.validate().expect("--mode rpc validates");
    }

    #[test]
    fn rpc_subcommand_help_describes_the_rpc_server() {
        let command = Cli::command();
        let rpc = command
            .find_subcommand("rpc")
            .expect("rpc subcommand exists");
        let about = rpc
            .get_about()
            .expect("rpc subcommand about")
            .to_string();
        assert!(
            about.contains("rpi headless RPC server"),
            "rpc subcommand help must describe the RPC server: {about}"
        );
    }

    #[test]
    fn parses_mcp_subcommands() {
        let list = Cli::try_parse_from(["rpi", "mcp", "list"]).expect("mcp list");
        assert!(matches!(
            list.command,
            Some(Command::Mcp {
                command: McpCommand::List { local: false }
            })
        ));
        let list_local = Cli::try_parse_from(["rpi", "mcp", "list", "--local"])
            .expect("mcp list --local");
        assert!(matches!(
            list_local.command,
            Some(Command::Mcp {
                command: McpCommand::List { local: true }
            })
        ));

        let import = Cli::try_parse_from(["rpi", "mcp", "import"]).expect("mcp import");
        assert!(matches!(
            import.command,
            Some(Command::Mcp {
                command: McpCommand::Import {
                    source: None,
                    file: None,
                    local: false,
                    force: false,
                }
            })
        ));

        let import_cursor = Cli::try_parse_from([
            "rpi",
            "mcp",
            "import",
            "--source",
            "cursor",
            "--file",
            "/tmp/mcp.json",
            "--local",
            "--force",
        ])
        .expect("mcp import --source cursor --file ... --local --force");
        assert!(matches!(
            import_cursor.command,
            Some(Command::Mcp {
                command: McpCommand::Import {
                    source: Some(McpImportSourceArg::Cursor),
                    file: Some(path),
                    local: true,
                    force: true,
                }
            }) if path == PathBuf::from("/tmp/mcp.json")
        ));

        // `rpi mcp` without a subcommand is a parse error.
        assert!(Cli::try_parse_from(["rpi", "mcp"]).is_err());
        // `rpi mcp import --source` must reject unknown formats.
        assert!(Cli::try_parse_from(["rpi", "mcp", "import", "--source", "grok"]).is_err());
    }

    #[test]
    fn cwd_is_global_for_sessions_subcommand() {
        let before =
            Cli::try_parse_from(["rpi", "--cwd", "workspace-a", "sessions"]).expect("before");
        assert_eq!(
            before.cwd.as_deref(),
            Some(PathBuf::from("workspace-a").as_path())
        );
        assert!(matches!(before.command, Some(Command::Sessions)));

        let after = Cli::try_parse_from(["rpi", "sessions", "-C", "workspace-b"]).expect("after");
        assert_eq!(
            after.cwd.as_deref(),
            Some(PathBuf::from("workspace-b").as_path())
        );
    }

    #[test]
    fn profile_flag_is_global_and_mirrors_session_dir() {
        // `--profile` is a global arg like `--session-dir`: accepted before or
        // after a subcommand.
        let before =
            Cli::try_parse_from(["rpi", "--profile", "work", "sessions"]).expect("before");
        assert_eq!(before.profile.as_deref(), Some("work"));
        assert!(matches!(before.command, Some(Command::Sessions)));

        let after = Cli::try_parse_from(["rpi", "sessions", "--profile", "work"]).expect("after");
        assert_eq!(after.profile.as_deref(), Some("work"));
        assert!(matches!(after.command, Some(Command::Sessions)));

        let plain = Cli::try_parse_from(["rpi"]).expect("plain");
        assert_eq!(plain.profile, None, "--profile must default to none");

        let with_session_dir =
            Cli::try_parse_from(["rpi", "--profile", "work", "--session-dir", "sessions"])
                .expect("profile plus session dir");
        assert_eq!(with_session_dir.profile.as_deref(), Some("work"));
        assert_eq!(
            with_session_dir.session_dir.as_deref(),
            Some(PathBuf::from("sessions").as_path())
        );
    }

    #[test]
    fn profile_names_validate_to_actionable_errors() {
        for name in ["work", "my-profile", "my_profile", "work2", "A-Z_09", "default"] {
            validate_profile_name(name)
                .unwrap_or_else(|error| panic!("{name:?} must be valid: {error}"));
        }
        let empty = validate_profile_name("").expect_err("empty name must be rejected");
        assert!(
            empty.contains("cannot be empty"),
            "empty-name error must be actionable: {empty}"
        );
        for name in ["a/b", "a b", "a.b", "work:high", "a\\b", "über", "a,b"] {
            let error = validate_profile_name(name)
                .expect_err("invalid profile name must be rejected");
            assert!(
                error.contains("profile name") && error.contains("letters, digits"),
                "{name:?} error must be actionable: {error}"
            );
        }
        // 64 chars is the maximum; 65 is rejected.
        let max = "a".repeat(MAX_PROFILE_NAME_LENGTH);
        validate_profile_name(&max).expect("64-char name is valid");
        let too_long = "a".repeat(MAX_PROFILE_NAME_LENGTH + 1);
        let error = validate_profile_name(&too_long).expect_err("65-char name is invalid");
        assert!(
            error.contains("exceeds") && error.contains("64"),
            "length error must be actionable: {error}"
        );
    }

    #[test]
    fn cli_validate_rejects_invalid_profile_but_accepts_default_and_empty() {
        for args in [
            ["rpi", "--profile", "bad/name"].as_slice(),
            ["rpi", "--profile", "has space"].as_slice(),
        ] {
            let cli = Cli::try_parse_from(args).expect("parse for validation");
            let error = cli.validate().expect_err("invalid profile must fail");
            assert!(
                error.contains("--profile") && error.contains("letters, digits"),
                "validation error must be actionable: {error}"
            );
        }
        let cli = Cli::try_parse_from(["rpi", "--profile", "x".repeat(65).as_str()])
            .expect("parse for validation");
        let error = cli.validate().expect_err("overlong profile must fail");
        assert!(
            error.contains("--profile") && error.contains("exceeds 64"),
            "validation error must be actionable: {error}"
        );
        for args in [
            ["rpi", "--profile", "default"].as_slice(),
            ["rpi", "--profile", ""].as_slice(),
            ["rpi", "--profile", "work"].as_slice(),
        ] {
            let cli = Cli::try_parse_from(args).expect("parse for validation");
            cli.validate().expect("valid profile must pass validation");
        }
    }

    #[test]
    fn profile_flag_appears_in_help() {
        let mut buf = Vec::new();
        write_completion(CompletionShell::Bash, &mut buf);
        let script = String::from_utf8(buf).expect("completion script is valid UTF-8");
        assert!(script.contains("--profile"), "completion mentions --profile");
    }

    #[test]
    fn listen_flags_parse_and_enforce_remote_opt_in_dependencies() {
        let cli = Cli::try_parse_from(["rpi", "--listen", "127.0.0.1:0"])
            .expect("parse listen loopback");
        assert_eq!(
            cli.listen.as_ref().map(std::net::SocketAddr::to_string),
            Some("127.0.0.1:0".to_owned())
        );
        cli.validate().expect("loopback listener validates");

        let prompt = Cli::try_parse_from([
            "rpi",
            "--listen",
            "127.0.0.1:0",
            "prompt must use Web RPC",
        ])
        .expect("parse listener prompt rejection fixture");
        assert_eq!(
            prompt.validate().expect_err("listener positional prompt must fail"),
            "--listen is Web-only and cannot be combined with positional prompts; submit prompts through /web, /ws, or /rpc"
        );
        assert!(cli.listen_token_file.is_none());
        assert!(!cli.listen_allow_insecure_remote);

        let with_remote_opt_in = Cli::try_parse_from([
            "rpi",
            "--listen",
            "0.0.0.0:0",
            "--listen-token-file",
            "token-file",
            "--listen-allow-insecure-remote",
        ])
        .expect("parse authenticated remote listen opt-in");
        assert_eq!(
            with_remote_opt_in.listen_token_file.as_deref(),
            Some(PathBuf::from("token-file").as_path())
        );
        assert!(with_remote_opt_in.listen_allow_insecure_remote);

        // The explicit remote opt-in no longer requires a token file:
        // tokenless LAN listening is an allowed (if unauthenticated) mode.
        let tokenless_remote = Cli::try_parse_from([
            "rpi",
            "--listen",
            "0.0.0.0:0",
            "--listen-allow-insecure-remote",
        ])
        .expect("parse tokenless remote listen opt-in");
        assert!(tokenless_remote.listen_allow_insecure_remote);
        assert!(tokenless_remote.listen_token_file.is_none());
        tokenless_remote
            .validate()
            .expect("tokenless remote opt-in must validate");

        for args in [
            ["rpi", "--listen-token-file", "token-file"].as_slice(),
            ["rpi", "--listen-allow-insecure-remote"].as_slice(),
        ] {
            assert!(
                Cli::try_parse_from(args).is_err(),
                "accepted incomplete listen dependency set: {args:?}"
            );
        }

        let mut direct = Cli::try_parse_from(["rpi", "--listen", "0.0.0.0:0"])
            .expect("parse direct validation fixture");
        direct.listen_allow_insecure_remote = true;
        direct
            .validate()
            .expect("validation must accept tokenless remote opt-in");
        direct.listen = None;
        assert_eq!(
            direct.validate().expect_err("validation must require listen"),
            "--listen-allow-insecure-remote requires --listen"
        );
    }

    #[test]
    fn listen_tls_flags_enforce_pairing_and_plaintext_conflicts() {
        // Default (no TLS flags): HTTPS with an auto-generated self-signed
        // certificate.
        let cli = Cli::try_parse_from(["rpi", "--listen", "127.0.0.1:0"])
            .expect("parse listen loopback");
        assert!(!cli.listen_plaintext);
        assert!(cli.listen_cert.is_none());
        assert!(cli.listen_key.is_none());
        cli.validate().expect("default listener validates");

        // --listen-plaintext is the explicit HTTP opt-out.
        let plaintext = Cli::try_parse_from([
            "rpi",
            "--listen",
            "127.0.0.1:0",
            "--listen-plaintext",
        ])
        .expect("parse plaintext listener");
        assert!(plaintext.listen_plaintext);
        plaintext
            .validate()
            .expect("plaintext listener validates");

        // A certificate pair is consumed as a unit.
        let tls = Cli::try_parse_from([
            "rpi",
            "--listen",
            "127.0.0.1:0",
            "--listen-cert",
            "cert.pem",
            "--listen-key",
            "key.pem",
        ])
        .expect("parse TLS listener");
        assert_eq!(
            tls.listen_cert.as_deref(),
            Some(PathBuf::from("cert.pem").as_path())
        );
        assert_eq!(
            tls.listen_key.as_deref(),
            Some(PathBuf::from("key.pem").as_path())
        );
        assert!(!tls.listen_plaintext);
        tls.validate().expect("TLS listener validates");

        // A lone cert or key is rejected, as is combining the explicit
        // plaintext opt-out with a certificate pair, and the certificate
        // pair requires --listen (these flags only govern the listener).
        for args in [
            ["rpi", "--listen", "127.0.0.1:0", "--listen-cert", "cert.pem"].as_slice(),
            ["rpi", "--listen", "127.0.0.1:0", "--listen-key", "key.pem"].as_slice(),
            [
                "rpi",
                "--listen",
                "127.0.0.1:0",
                "--listen-plaintext",
                "--listen-cert",
                "cert.pem",
                "--listen-key",
                "key.pem",
            ]
            .as_slice(),
            ["rpi", "--listen-cert", "cert.pem", "--listen-key", "key.pem"].as_slice(),
        ] {
            assert!(
                Cli::try_parse_from(args).is_err(),
                "accepted invalid TLS listen flag set: {args:?}"
            );
        }
    }

    #[test]
    fn listen_advertised_origin_parses_strict_origins_and_rejects_non_origins() {
        let with_root = Cli::try_parse_from([
            "rpi",
            "--listen",
            "0.0.0.0:0",
            "--listen-advertised-origin",
            "https://collab.example:8443/",
        ])
        .expect("parse advertised origin with root slash");
        with_root
            .validate()
            .expect("root-slash origin must validate");
        assert_eq!(
            crate::modes::listen::parse_advertised_origin(
                with_root.listen_advertised_origin.as_deref().expect("origin")
            )
            .expect("normalize root-slash origin"),
            "https://collab.example:8443"
        );

        let plaintext = Cli::try_parse_from([
            "rpi",
            "--listen",
            "0.0.0.0:0",
            "--listen-advertised-origin",
            "http://127.0.0.1:8765",
        ])
        .expect("parse plaintext advertised origin");
        plaintext
            .validate()
            .expect("plaintext origin must validate");

        for bad in [
            "ftp://collab.example",
            "http://",
            "http://host/path",
            "http://host?x=1",
            "http://host#fragment",
            "http://user:pass@host",
            "http://ho st",
            "http://host:99999",
        ] {
            let cli = Cli::try_parse_from([
                "rpi",
                "--listen",
                "0.0.0.0:0",
                "--listen-advertised-origin",
                bad,
            ])
            .expect("parse origin candidate");
            let error = cli
                .validate()
                .expect_err("strict origin validation must reject non-origin");
            assert!(
                error.contains("--listen-advertised-origin"),
                "{bad:?} must produce a flag-named error: {error}"
            );
        }

        assert!(
            Cli::try_parse_from(["rpi", "--listen-advertised-origin", "http://host"]).is_err(),
            "--listen-advertised-origin requires --listen"
        );
    }

    #[test]
    fn listen_rejects_structured_modes_and_subcommands() {
        let rpc = Cli::try_parse_from(["rpi", "--mode", "rpc", "--listen", "127.0.0.1:0"])
            .expect("parse rpc listen");
        assert!(rpc.validate().is_err());
        let json = Cli::try_parse_from(["rpi", "--mode", "json", "--listen", "127.0.0.1:0"])
            .expect("parse json listen");
        assert!(json.validate().is_err());
        let sessions = Cli::try_parse_from(["rpi", "sessions", "--listen", "127.0.0.1:0"]);
        assert!(sessions.is_err(), "subcommand plus listen parses");
        let print_mode = Cli::try_parse_from(["rpi", "--print", "--listen", "127.0.0.1:0"])
            .expect("parse print listen");
        assert!(print_mode.validate().is_err());
    }

    #[test]
    fn listen_token_file_requires_non_empty_path() {
        // Contract: clap's `PathBuf` value parser rejects empty values with
        // `ErrorKind::InvalidValue`, so `--listen-token-file ''` fails at parse
        // time before `validate()` runs (the empty-path guard there is defensive).
        let error = Cli::try_parse_from([
            "rpi",
            "--listen",
            "127.0.0.1:0",
            "--listen-token-file",
            "",
        ])
        .expect_err("empty token path should be rejected at parse time");
        assert_eq!(error.kind(), clap::error::ErrorKind::InvalidValue);
    }

    #[test]
    fn completion_parses_supported_shells_and_rejects_others() {
        for (wire, expected) in [("bash", CompletionShell::Bash), ("zsh", CompletionShell::Zsh), ("fish", CompletionShell::Fish)] {
            let cli = Cli::try_parse_from(["rpi", "completion", wire])
                .expect("parse completion {wire}");
            assert!(
                matches!(cli.command, Some(Command::Completion { shell }) if shell == expected),
                "expected parsed shell {wire}"
            );
        }
        assert!(Cli::try_parse_from(["rpi", "completion"]).is_err(), "missing shell accepted");
        assert!(
            Cli::try_parse_from(["rpi", "completion", "powershell"]).is_err(),
            "powershell accepted"
        );
        assert!(
            Cli::try_parse_from(["rpi", "completion", "elvish"]).is_err(),
            "elvish accepted"
        );
    }

    #[test]
    fn completion_generates_non_empty_script_per_shell() {
        for shell in [CompletionShell::Bash, CompletionShell::Zsh, CompletionShell::Fish] {
            let mut buf = Vec::new();
            write_completion(shell, &mut buf);
            let script = String::from_utf8(buf).expect("completion script is valid UTF-8");
            assert!(!script.is_empty(), "{shell:?} generated empty script");
            assert!(script.contains("rpi"), "{shell:?} script does not mention rpi");
            assert!(script.contains("--help"), "{shell:?} script does not mention --help");
        }
    }

    #[test]
    fn top_level_export_flag_parses_like_export_subcommand() {
        let cli = Cli::try_parse_from(["rpi", "--export", "sessions/a.jsonl"])
            .expect("parse --export");
        assert!(cli.command.is_none(), "--export must not select a subcommand");
        assert_eq!(cli.export, Some(PathBuf::from("sessions/a.jsonl")));
        assert!(cli.output.is_none(), "--output must default to none");
        assert!(!cli.jsonl, "--jsonl must default to false");

        let cli = Cli::try_parse_from([
            "rpi", "--export", "sessions/a.jsonl", "-o", "out.html", "--jsonl",
        ])
        .expect("parse --export with output and jsonl");
        assert_eq!(cli.export, Some(PathBuf::from("sessions/a.jsonl")));
        assert_eq!(cli.output, Some(PathBuf::from("out.html")));
        assert!(cli.jsonl, "--jsonl must be honored");
    }

    #[test]
    fn top_level_export_flag_conflicts_with_subcommand_and_requires_export() {
        // The flag surface mirrors the `export` subcommand; combining them is
        // ambiguous and must fail validation (clap models subcommands as
        // separate commands, so the mutual exclusion lives in `validate`).
        let mixed = Cli::try_parse_from(["rpi", "--export", "x", "export", "y"])
            .expect("parse flag plus subcommand");
        assert!(mixed.validate().is_err(), "flag plus subcommand must fail");
        let sessions = Cli::try_parse_from(["rpi", "--export", "x", "sessions"])
            .expect("parse flag plus sessions subcommand");
        assert!(sessions.validate().is_err(), "flag plus sessions must fail");
        // -o/--output and --jsonl only make sense with --export.
        assert!(Cli::try_parse_from(["rpi", "--output", "out.html"]).is_err());
        assert!(Cli::try_parse_from(["rpi", "-o", "out.html"]).is_err());
        assert!(Cli::try_parse_from(["rpi", "--jsonl"]).is_err());
    }

    #[test]
    fn login_and_logout_accept_optional_scope_label() {
        let login = Cli::try_parse_from(["rpi", "login", "anthropic", "--scope", "work"])
            .expect("parse login with scope");
        match login.command {
            Some(Command::Login { provider, scope }) => {
                assert_eq!(provider.as_deref(), Some("anthropic"));
                assert_eq!(scope.as_deref(), Some("work"));
            }
            other => panic!("expected login command, got {other:?}"),
        }

        let logout = Cli::try_parse_from(["rpi", "logout", "--scope", "personal", "xai"])
            .expect("parse logout with scope before provider");
        match logout.command {
            Some(Command::Logout { provider, scope }) => {
                assert_eq!(provider.as_deref(), Some("xai"));
                assert_eq!(scope.as_deref(), Some("personal"));
            }
            other => panic!("expected logout command, got {other:?}"),
        }

        let plain = Cli::try_parse_from(["rpi", "login", "anthropic"]).expect("parse login");
        match plain.command {
            Some(Command::Login { provider, scope }) => {
                assert_eq!(provider.as_deref(), Some("anthropic"));
                assert_eq!(scope, None, "scope must default to none");
            }
            other => panic!("expected login command, got {other:?}"),
        }
    }
}
