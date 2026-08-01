//! Command-line argument parsing for the `pi` binary.
//!
//! Mirrors the Go upstream flag surface: top-level flags drive the agent run
//! path (print mode or interactive REPL), while `models`, `sessions`, and
//! `import-session` are first-class subcommands. `--version` is provided by
//! clap for release smoke tests.

use std::ffi::OsString;
use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

/// `pi` — Rust port of the pi coding agent.
#[derive(Debug, Clone, Parser)]
#[command(
    name = "pi",
    version,
    about = "pi - Rust port of the pi coding agent",
    long_about = None,
    args_override_self = true,
)]
pub struct Cli {
    /// Subcommand dispatch. When absent, the top-level flags drive a run.
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Provider id used with --model.
    #[arg(long, value_name = "PROVIDER", requires = "model")]
    pub provider: Option<String>,

    /// Model spec (provider/id or bare id).
    #[arg(short = 'm', long, value_name = "SPEC")]
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
    #[arg(short = 'c', long = "continue", conflicts_with_all = ["resume", "resume_codex", "session", "session_id", "fork", "no_session"])]
    pub continue_latest: bool,

    /// Resume a specific session file (legacy pi-rs path form).
    #[arg(long, value_name = "PATH", conflicts_with_all = ["continue_latest", "resume_codex", "session", "session_id", "fork", "no_session"])]
    pub resume: Option<PathBuf>,

    /// Import a Codex session (path or source id) then resume it.
    #[arg(long, value_name = "PATH_OR_ID", conflicts_with_all = ["continue_latest", "resume", "session", "session_id", "fork", "no_session"])]
    pub resume_codex: Option<String>,

    /// Open a session by file path, exact id, or unambiguous id prefix.
    #[arg(long, value_name = "PATH_OR_ID", conflicts_with_all = ["continue_latest", "resume", "resume_codex", "session_id", "fork", "no_session"])]
    pub session: Option<String>,

    /// Open an exact project session id, creating it when absent.
    #[arg(long, value_name = "ID", conflicts_with_all = ["continue_latest", "resume", "resume_codex", "session", "no_session"])]
    pub session_id: Option<String>,

    /// Fork a session by file path, exact id, or unambiguous id prefix.
    #[arg(long, value_name = "PATH_OR_ID", conflicts_with_all = ["continue_latest", "resume", "resume_codex", "session", "no_session"])]
    pub fork: Option<String>,

    /// Override the directory used for session storage and id lookup.
    #[arg(long, value_name = "DIR")]
    pub session_dir: Option<PathBuf>,

    /// Do not persist a session file for this run.
    #[arg(long, conflicts_with_all = ["continue_latest", "resume", "resume_codex", "session", "session_id", "fork"])]
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
    #[arg(long)]
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

    /// Prompt turns. On a terminal these initialize the interactive UI; in
    /// print/structured modes each positional is sent as a separate turn.
    #[arg(value_name = "PROMPT")]
    pub prompt: Vec<String>,

    /// Reserved extension flags after extension registration. The current
    /// process-extension protocol does not register CLI flags, so clap rejects
    /// unknown long options rather than accepting and ignoring them.
    #[arg(skip)]
    pub extension_cli_flags: Vec<(String, Option<String>)>,
}

/// Headless application adapters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Mode {
    Text,
    Json,
    Rpc,
}

/// First-class subcommands.
#[derive(Debug, Clone, Subcommand)]
pub enum Command {
    /// Configure an API key or provider subscription.
    Login {
        /// Provider id; omit in an interactive terminal to choose from a list.
        #[arg(value_name = "PROVIDER")]
        provider: Option<String>,
    },
    /// Remove the stored credential for one provider.
    Logout {
        /// Provider id; omit in an interactive terminal to choose from configured providers.
        #[arg(value_name = "PROVIDER")]
        provider: Option<String>,
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
    /// Configure enabled package resources for global or project scope.
    Config {
        /// Edit project-local settings instead of global settings.
        #[arg(short = 'l', long)]
        local: bool,
    },
    /// Update pi itself, configured extensions, or model catalogs.
    Update {
        /// Explicitly update the managed pi installation (also the default with no target).
        #[arg(long = "self", conflicts_with_all = ["package", "extension", "models", "all"])]
        self_update: bool,
        /// Update every configured extension; combine with --self to update both.
        #[arg(long, conflicts_with_all = ["extension", "models", "all"])]
        extensions: bool,
        /// Update pi and every configured extension.
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
        /// Update one configured extension by source identity; `self` and `pi` select pi itself.
        #[arg(value_name = "PACKAGE", conflicts_with_all = ["all", "models", "extension"])]
        package: Option<String>,
    },
    /// Manage a configured llama.cpp router and local GGUF downloads.
    Llama {
        #[command(subcommand)]
        command: LlamaCommand,
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
                .is_some_and(|source| matches!(source, "self" | "pi"));
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
                return Err("--force only applies to pi self-update targets".to_owned());
            }
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
        let empty = Cli::try_parse_from(["pi", ""]).expect("parse empty positional");
        assert!(empty.prompt_text().is_empty());
        assert!(!empty.is_print_mode());
        let cli = Cli::try_parse_from(["pi", "hello", "world"]).expect("parse prompts");
        assert_eq!(cli.prompt, ["hello", "world"]);
        assert_eq!(cli.prompt_text(), "hello world");
        assert!(!cli.is_print_mode());
    }

    #[test]
    fn explicit_print_flag_forces_print_mode() {
        let cli = Cli::try_parse_from(["pi", "-p"]).expect("parse -p");
        assert!(cli.is_print_mode());
        let cli = Cli::try_parse_from(["pi", "--print", ""]).expect("parse --print empty");
        assert!(cli.is_print_mode());
    }

    #[test]
    fn parses_core_parity_flags_and_repeats() {
        let cli = Cli::try_parse_from([
            "pi", "--provider", "openai", "--model", "gpt-5", "--models",
            "openai/*,*sonnet*:high", "--system-prompt", "system.txt",
            "--append-system-prompt", "one", "--append-system-prompt", "two",
            "--name", "named", "--session-id", "session_1", "--session-dir",
            "sessions", "--thinking", "high", "--mode", "text", "--offline",
            "--verbose", "-t", "read,custom", "-xt", "bash,task", "-e", "one-ext",
            "--extension", "two-ext", "--skill", "skill-a", "--skill", "skill-b",
            "--prompt-template", "prompt-a", "--prompt-template", "prompt-b",
            "--theme", "theme-a", "--theme", "theme-b",
        ]).expect("parse parity flags");
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
        assert!(cli.offline && cli.verbose);
    }

    #[test]
    fn parses_disable_aliases_session_paths_and_optional_model_search() {
        let cli = Cli::try_parse_from([
            "pi", "-ne", "-ns", "-np", "--no-themes", "-nc", "-nbt",
            "--session", "sessions/a.jsonl",
        ]).expect("disable aliases");
        assert!(cli.no_extensions && cli.no_skills && cli.no_prompt_templates);
        assert!(cli.no_themes && cli.no_context_files && cli.no_builtin_tools);
        assert_eq!(cli.session.as_deref(), Some("sessions/a.jsonl"));
        let all = Cli::try_parse_from(["pi", "--list-models"]).expect("list all");
        assert_eq!(all.list_models.as_deref(), Some(""));
        let searched = Cli::try_parse_from(["pi", "--list-models", "sonnet"])
            .expect("list search");
        assert_eq!(searched.list_models.as_deref(), Some("sonnet"));
    }

    #[test]
    fn rejects_conflicts_empty_values_and_unknown_flags() {
        for args in [
            ["pi", "--session", "id", "--fork", "other"].as_slice(),
            ["pi", "--no-session", "--continue"].as_slice(),
            ["pi", "--no-tools", "--no-builtin-tools"].as_slice(),
            ["pi", "--approve", "--no-approve"].as_slice(),
        ] {
            assert!(Cli::try_parse_from(args).is_err(), "accepted conflict: {args:?}");
        }
        for args in [
            ["pi", "--name", ""].as_slice(),
            ["pi", "--session-id", ""].as_slice(),
            ["pi", "--models", "a,,b"].as_slice(),
            ["pi", "--tools", "read,,bash"].as_slice(),
        ] {
            let cli = Cli::try_parse_from(args).expect("parse for validation");
            assert!(cli.validate().is_err(), "accepted empty value: {args:?}");
        }
        assert!(Cli::try_parse_from(["pi", "--provider", "openai"]).is_err());
        assert!(Cli::try_parse_from(["pi", "--unknown-extension-flag"]).is_err());
    }
    #[test]
    fn parses_package_aliases_update_targets_and_global_trust_flags() {
        let uninstall = Cli::try_parse_from(["pi", "uninstall", "package", "--approve"])
            .expect("uninstall alias and trailing trust flag");
        assert!(uninstall.approve);
        assert!(matches!(uninstall.command, Some(Command::Remove { ref source, .. }) if source == "package"));

        let no_approve = Cli::try_parse_from(["pi", "config", "-na"])
            .expect("no-approve alias after command");
        assert!(no_approve.no_approve);

        for args in [
            ["pi", "update", "--all"].as_slice(),
            ["pi", "update", "--models"].as_slice(),
            ["pi", "update", "--extension", "package"].as_slice(),
            ["pi", "update", "self"].as_slice(),
            ["pi", "update", "pi", "--extensions"].as_slice(),
        ] {
            let cli = Cli::try_parse_from(args).expect("parse update target");
            cli.validate().expect("validate update target");
        }
    }


    #[test]
    fn validates_update_target_combinations() {
        let all = Cli::try_parse_from(["pi", "update", "--all", "--force"])
            .expect("parse all force");
        all.validate().expect("all includes self");

        let alias = Cli::try_parse_from([
            "pi",
            "update",
            "self",
            "--extensions",
            "--force",
        ])
        .expect("parse self alias");
        alias.validate().expect("self alias includes self");

        let extensions = Cli::try_parse_from(["pi", "update", "--extensions", "--force"])
            .expect("parse extensions force");
        assert!(extensions.validate().is_err());

        let package = Cli::try_parse_from(["pi", "update", "git:example/repo", "--extensions"])
            .expect("parse package extensions");
        assert!(package.validate().is_err());
    }
    #[test]
    fn cwd_is_global_for_sessions_subcommand() {
        let before =
            Cli::try_parse_from(["pi", "--cwd", "workspace-a", "sessions"]).expect("before");
        assert_eq!(
            before.cwd.as_deref(),
            Some(PathBuf::from("workspace-a").as_path())
        );
        assert!(matches!(before.command, Some(Command::Sessions)));

        let after = Cli::try_parse_from(["pi", "sessions", "-C", "workspace-b"]).expect("after");
        assert_eq!(
            after.cwd.as_deref(),
            Some(PathBuf::from("workspace-b").as_path())
        );
        assert!(matches!(after.command, Some(Command::Sessions)));
    }
}
