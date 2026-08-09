//! `rpi doctor`, `rpi setup`, and `rpi dashboard` subcommands.
//!
//! These are read-only diagnostics and guidance commands. They never print
//! secret material: `auth.json` and `models.json` are parsed (size-bounded)
//! for shape and counts only, and their contents are never echoed — auth
//! presence is reported as provider names and the file path.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::models_config::{auth_json_path, models_json_path};

/// rpi version string (compile-time package version).
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Mirrors `models_config::MAX_CONFIG_FILE_BYTES` so the read-only doctor
/// parse agrees with the real loader's bound.
const MAX_CONFIG_FILE_BYTES: u64 = 8 * 1024 * 1024;

/// How many objective characters the dashboard prints before truncating.
const MAX_OBJECTIVE_CHARS: usize = 80;

/// Example `models.json` contents shown by `rpi setup` on a terminal.
const MODELS_EXAMPLE: &str = r#"{
  "providers": {
    "my-provider": {
      "baseUrl": "https://api.example.com/v1",
      "models": [
        { "id": "my-model", "name": "My Model" }
      ]
    }
  }
}"#;

/// Example `auth.json` contents shown by `rpi setup` on a terminal.
const AUTH_EXAMPLE: &str = r#"{
  "my-provider": {
    "type": "api_key",
    "key": "your-api-key-here"
  }
}"#;

/// Outcome of one diagnostic check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Status {
    Pass,
    Warn,
    Fail,
}

impl Status {
    const fn label(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Warn => "WARN",
            Self::Fail => "FAIL",
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Warn => "warn",
            Self::Fail => "fail",
        }
    }
}

/// One named diagnostic with a status and a human-readable detail line.
#[derive(Clone, Debug)]
struct Check {
    name: &'static str,
    status: Status,
    detail: String,
}

impl Check {
    fn new(name: &'static str, status: Status, detail: impl Into<String>) -> Self {
        Self {
            name,
            status,
            detail: detail.into(),
        }
    }

    fn pass(name: &'static str, detail: impl Into<String>) -> Self {
        Self::new(name, Status::Pass, detail)
    }

    fn warn(name: &'static str, detail: impl Into<String>) -> Self {
        Self::new(name, Status::Warn, detail)
    }

    fn fail(name: &'static str, detail: impl Into<String>) -> Self {
        Self::new(name, Status::Fail, detail)
    }
}

/// `rpi doctor [--json]`.
///
/// Runs every environment check and reports PASS/WARN/FAIL per check. Always
/// exits successfully: this is a diagnostic, not a gate.
pub fn doctor_command(cli: &crate::Cli, json: bool) -> Result<()> {
    let stdout = std::io::stdout();
    let mut writer = stdout.lock();
    doctor_into(cli, json, &mut writer)
}

/// Render the doctor report into `writer` (shared with tests).
fn doctor_into(cli: &crate::Cli, json: bool, writer: &mut impl Write) -> Result<()> {
    let checks = collect_doctor_checks(cli);
    if json {
        write_doctor_json(writer, &checks)
    } else {
        write_doctor_text(writer, &checks);
        Ok(())
    }
}

fn collect_doctor_checks(cli: &crate::Cli) -> Vec<Check> {
    let mut checks = Vec::new();

    // Agent directory (parent of models.json / auth.json).
    let agent = agent_dir();
    checks.push(agent_dir_check(agent.as_deref()));

    // models.json: bounded, read-only parse. The global snapshot is never
    // touched, so running doctor cannot disturb an in-process configuration.
    let models_path = models_json_path();
    checks.push(models_json_check(models_path.as_deref()));

    // auth.json: presence and provider names only — never contents. The
    // loader's errors are already value-free (path + shape), so they are safe
    // to surface verbatim.
    let auth_path = auth_json_path();
    checks.push(auth_json_check(auth_path.as_deref()));

    // Working directory + settings/resources graph. A settings failure is its
    // own check; session-dir resolution degrades to the default so the rest
    // of the diagnostics still run in a broken environment.
    let cwd = match resolve_cwd(cli) {
        Ok(cwd) => cwd,
        Err(error) => {
            checks.push(Check::fail("working directory", format!("{error:#}")));
            return checks;
        }
    };
    let (settings_status, session_dir) = match load_settings_session_dir(&cwd, cli) {
        Ok(settings_session_dir) => {
            match crate::session_run::effective_session_dir(
                &cwd,
                cli.session_dir.as_deref(),
                settings_session_dir.as_deref(),
            ) {
                Ok(dir) => (Check::pass("settings", "loaded"), dir),
                Err(error) => (
                    Check::fail("settings", format!("{error:#}")),
                    fallback_session_dir(cli, &cwd),
                ),
            }
        }
        Err(error) => (
            Check::fail("settings", format!("{error:#}")),
            fallback_session_dir(cli, &cwd),
        ),
    };
    checks.push(settings_status);

    // Session directory writability probe (create + write + remove).
    match probe_writable(&session_dir) {
        Ok(()) => checks.push(Check::pass(
            "session dir",
            format!("{} (writable)", session_dir.display()),
        )),
        Err(error) => checks.push(Check::fail(
            "session dir",
            format!("{}: {error:#}", session_dir.display()),
        )),
    }

    // git binary availability.
    match git_version() {
        Ok(version) => checks.push(Check::pass("git", version)),
        Err(error) => checks.push(Check::fail("git", format!("{error}"))),
    }

    // Providers configured, from the models snapshot plus auth credentials.
    checks.push(providers_check(
        configured_providers(models_path.as_deref()),
        authenticated_providers(auth_path.as_deref()),
    ));

    checks
}

/// The agent-dir diagnostic: PASS with the resolved path, FAIL when no agent
/// directory can be resolved.
fn agent_dir_check(agent_dir: Option<&Path>) -> Check {
    match agent_dir {
        Some(dir) => Check::pass("agent dir", dir.display().to_string()),
        None => Check::fail(
            "agent dir",
            "cannot resolve (set HOME or PI_CODING_AGENT_DIR)",
        ),
    }
}

/// The models.json diagnostic: dry-parse the file (bounded, comment-stripped)
/// and report provider/model counts. Errors carry the path and shape only,
/// never file contents.
fn models_json_check(path: Option<&Path>) -> Check {
    match path {
        Some(path) => match dry_parse_models_json(path) {
            Ok((providers, models)) => Check::pass(
                "models.json",
                format!("{} ({providers} providers, {models} models)", path.display()),
            ),
            Err(error) => Check::fail("models.json", format!("{}: {error:#}", path.display())),
        },
        None => Check::fail(
            "models.json",
            "cannot resolve (set HOME or PI_CODING_AGENT_DIR)",
        ),
    }
}

/// The auth.json diagnostic: presence and provider names only — never
/// credential contents.
fn auth_json_check(path: Option<&Path>) -> Check {
    match path {
        Some(path) => match pi_coding::load_credentials(path) {
            Ok(credentials) if !credentials.is_empty() => {
                let names = credentials.keys().cloned().collect::<Vec<_>>().join(", ");
                Check::pass(
                    "auth.json",
                    format!(
                        "{} ({} provider(s): {names})",
                        path.display(),
                        credentials.len()
                    ),
                )
            }
            Ok(_) => Check::warn(
                "auth.json",
                format!("no credentials (empty {})", path.display()),
            ),
            Err(error) => Check::fail("auth.json", format!("{}: {error:#}", path.display())),
        },
        None => Check::warn(
            "auth.json",
            "not configured (cannot resolve auth.json path)",
        ),
    }
}

/// The providers summary: counts from the models snapshot and auth.json.
fn providers_check(models_providers: usize, auth_providers: usize) -> Check {
    Check::pass(
        "providers",
        format!(
            "{models_providers} configured in models.json, {auth_providers} authenticated in auth.json"
        ),
    )
}

/// Provider count from a dry models.json parse (0 when unreadable).
fn configured_providers(path: Option<&Path>) -> usize {
    path.and_then(|path| dry_parse_models_json(path).ok())
        .map_or(0, |(providers, _)| providers)
}

/// Authenticated provider count from auth.json (0 when unreadable).
fn authenticated_providers(path: Option<&Path>) -> usize {
    path.filter(|path| path.is_file())
        .and_then(|path| pi_coding::load_credentials(path).ok())
        .map_or(0, |credentials| credentials.len())
}

/// Session-dir fallback used when settings or resolution fail: an explicit
/// CLI `--session-dir` wins, then the per-cwd default.
fn fallback_session_dir(cli: &crate::Cli, cwd: &Path) -> PathBuf {
    cli.session_dir
        .clone()
        .unwrap_or_else(|| pi_coding::default_session_dir(cwd))
}

fn write_doctor_text<W: Write>(writer: &mut W, checks: &[Check]) {
    writeln!(writer, "rpi doctor").expect("writing doctor report");
    writeln!(writer).expect("writing doctor report");
    writeln!(writer, "rpi version: {VERSION}").expect("writing doctor report");
    writeln!(writer).expect("writing doctor report");
    for check in checks {
        writeln!(
            writer,
            "{:<6}{:<13}{}",
            check.status.label(),
            check.name,
            check.detail
        )
        .expect("writing doctor report");
    }
}

fn write_doctor_json<W: Write>(writer: &mut W, checks: &[Check]) -> Result<()> {
    let report = serde_json::json!({
        "command": "doctor",
        "version": VERSION,
        "checks": checks.iter().map(|check| serde_json::json!({
            "name": check.name,
            "status": check.status.as_str(),
            "detail": check.detail,
        })).collect::<Vec<_>>(),
        "failed": checks.iter().filter(|check| check.status == Status::Fail).count(),
    });
    writeln!(writer, "{}", serde_json::to_string(&report)?)?;
    Ok(())
}

/// `rpi setup [--json]`.
///
/// Interactive terminals get guidance plus `models.json`/`auth.json` example
/// contents; piped/non-interactive stdout prints just the two paths; `--json`
/// emits the same information as a machine-readable object.
pub fn setup_command(json: bool) -> Result<()> {
    let stdout = std::io::stdout();
    let mut writer = stdout.lock();
    setup_into(json, agent_dir().as_deref(), &mut writer)
}

/// Render the setup output into `writer`. Interactive terminals get guidance
/// plus examples; piped/non-interactive stdout prints just the two paths;
/// `--json` emits the same information as a machine-readable object. The
/// agent directory is injectable for tests (the workspace forbids the unsafe
/// env mutation `std::env::set_var` requires in edition 2024).
fn setup_into(json: bool, agent_dir: Option<&Path>, writer: &mut impl Write) -> Result<()> {
    let interactive = !json && std::io::stdout().is_terminal();
    match agent_dir {
        Some(dir) => {
            let models = dir.join("models.json");
            let auth = dir.join("auth.json");
            if json {
                write_setup_json(writer, dir, &models, &auth)?;
            } else if interactive {
                write_setup_guidance(writer, dir, &models, &auth);
            } else {
                writeln!(writer, "{}", models.display())?;
                writeln!(writer, "{}", auth.display())?;
            }
        }
        None => {
            let message = "cannot resolve the agent directory (set HOME or PI_CODING_AGENT_DIR)";
            if json {
                writeln!(
                    writer,
                    "{}",
                    serde_json::to_string(&serde_json::json!({
                        "command": "setup",
                        "error": message,
                    }))?
                )?;
            } else {
                writeln!(writer, "agent directory: unavailable ({message})")?;
            }
        }
    }
    Ok(())
}

fn write_setup_guidance<W: Write>(writer: &mut W, dir: &Path, models: &Path, auth: &Path) {
    writeln!(writer, "rpi setup").expect("writing setup guidance");
    writeln!(writer).expect("writing setup guidance");
    writeln!(writer, "agent directory: {}", dir.display()).expect("writing setup guidance");
    writeln!(writer).expect("writing setup guidance");
    writeln!(writer, "Configure providers and models in:").expect("writing setup guidance");
    writeln!(writer, "  {}", models.display()).expect("writing setup guidance");
    writeln!(writer, "  {}", auth.display()).expect("writing setup guidance");
    writeln!(writer).expect("writing setup guidance");
    writeln!(writer, "models.json example:").expect("writing setup guidance");
    writeln!(writer, "{MODELS_EXAMPLE}").expect("writing setup guidance");
    writeln!(writer).expect("writing setup guidance");
    writeln!(writer, "auth.json example:").expect("writing setup guidance");
    writeln!(writer, "{AUTH_EXAMPLE}").expect("writing setup guidance");
    writeln!(writer).expect("writing setup guidance");
    writeln!(writer, "Environment variables (e.g. MY_PROVIDER_API_KEY) and `rpi login`")
        .expect("writing setup guidance");
    writeln!(writer, "are also supported for credentials. rpi never prints secrets.")
        .expect("writing setup guidance");
}

fn write_setup_json<W: Write>(writer: &mut W, dir: &Path, models: &Path, auth: &Path) -> Result<()> {
    let report = serde_json::json!({
        "command": "setup",
        "agentDir": dir,
        "modelsJson": models,
        "authJson": auth,
        "examples": {
            "modelsJson": MODELS_EXAMPLE,
            "authJson": AUTH_EXAMPLE,
        },
    });
    writeln!(writer, "{}", serde_json::to_string(&report)?)?;
    Ok(())
}

/// `rpi dashboard [--json]`.
///
/// Summarizes the session root for the working directory: session count,
/// latest session, goal state (if any), and available tools.
pub fn dashboard_command(cli: &crate::Cli, json: bool) -> Result<()> {
    let stdout = std::io::stdout();
    let mut writer = stdout.lock();
    dashboard_into(cli, json, &mut writer)
}

/// Render the dashboard summary into `writer` (shared with tests).
fn dashboard_into(cli: &crate::Cli, json: bool, writer: &mut impl Write) -> Result<()> {
    let cwd = resolve_cwd(cli)?;
    let settings_dir = load_settings_session_dir(&cwd, cli).ok().flatten();
    let session_dir =
        match crate::session_run::effective_session_dir(
            &cwd,
            cli.session_dir.as_deref(),
            settings_dir.as_deref(),
        ) {
            Ok(dir) => dir,
            Err(_) => fallback_session_dir(cli, &cwd),
        };

    let sessions = scan_session_files(&session_dir);
    let latest = sessions
        .iter()
        .max_by_key(|(_, modified)| *modified)
        .map(|(path, _)| path.clone());
    let latest_info = latest
        .as_deref()
        .and_then(|path| session_info(path).ok());
    let goal = latest
        .as_deref()
        .and_then(|path| read_goal_summary(path).ok())
        .flatten();
    let tools = pi_coding::TOOL_NAMES;

    if json {
        write_dashboard_json(
            writer,
            &cwd,
            &session_dir,
            sessions.len(),
            latest.as_deref().zip(latest_info.as_ref()),
            goal.as_ref(),
            tools,
        )
    } else {
        write_dashboard_text(
            writer,
            &cwd,
            &session_dir,
            sessions.len(),
            latest.as_deref().zip(latest_info.as_ref()),
            goal.as_ref(),
            tools,
        );
        Ok(())
    }
}

/// Goal summary shown by the dashboard: lifecycle, truncated objective, and
/// usage. Token budget is optional (unlimited when unset).
struct GoalSummary {
    lifecycle: &'static str,
    objective: String,
    tokens_used: u64,
    token_budget: Option<u64>,
}

/// Replay a session's goal journal and summarize the current goal, if any.
fn read_goal_summary(path: &Path) -> Result<Option<GoalSummary>> {
    let tree = pi_coding::load_session_tree(path)
        .with_context(|| format!("reading session {}", path.display()))?;
    let events = pi_coding::goal_events_from_session_tree(&tree)?;
    let runtime = pi_coding::GoalRuntime::from_events(&events)
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let Some(goal) = runtime.get().current else {
        return Ok(None);
    };
    Ok(Some(GoalSummary {
        lifecycle: goal_lifecycle_label(goal.lifecycle),
        // Credential-shaped text in the objective is redacted before it
        // reaches the dashboard text or JSON output.
        objective: pi_coding::redact::redact_secrets(&truncate(
            &goal.objective,
            MAX_OBJECTIVE_CHARS,
        )),
        tokens_used: goal.usage.tokens_used,
        token_budget: goal.token_budget,
    }))
}

fn goal_lifecycle_label(lifecycle: pi_coding::GoalLifecycle) -> &'static str {
    match lifecycle {
        pi_coding::GoalLifecycle::Active => "active",
        pi_coding::GoalLifecycle::Paused => "paused",
        pi_coding::GoalLifecycle::Completed => "completed",
        pi_coding::GoalLifecycle::Dropped => "dropped",
    }
}

fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let mut truncated: String = text.chars().take(max_chars).collect();
    truncated.push('…');
    truncated
}

/// Read the header timestamp and message count for one session file.
fn session_info(path: &Path) -> Result<(String, usize)> {
    let tree = pi_coding::load_session_tree(path)
        .with_context(|| format!("reading session {}", path.display()))?;
    let messages = tree.build_context(None).messages.len();
    Ok((tree.header.timestamp.clone(), messages))
}

/// Scan a session root for session files (top-level `*.jsonl`), returning
/// paths with their last-modified times.
fn scan_session_files(root: &Path) -> Vec<(PathBuf, std::time::SystemTime)> {
    let Ok(read_dir) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    read_dir
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "jsonl")
        })
        .filter_map(|entry| {
            let path = entry.path();
            let modified = std::fs::metadata(&path)
                .ok()
                .and_then(|metadata| metadata.modified().ok())?;
            Some((path, modified))
        })
        .collect()
}

fn write_dashboard_text<W: Write>(
    writer: &mut W,
    cwd: &Path,
    session_dir: &Path,
    sessions: usize,
    latest: Option<(&Path, &(String, usize))>,
    goal: Option<&GoalSummary>,
    tools: &[&str],
) {
    writeln!(writer, "rpi dashboard").expect("writing dashboard");
    writeln!(writer).expect("writing dashboard");
    writeln!(writer, "working directory: {}", cwd.display()).expect("writing dashboard");
    writeln!(writer, "session root:      {}", session_dir.display()).expect("writing dashboard");
    writeln!(writer, "sessions:          {sessions}").expect("writing dashboard");
    match latest {
        Some((path, (timestamp, messages))) => writeln!(
            writer,
            "latest:            {timestamp}  {messages} msgs  {}",
            path.display()
        )
        .expect("writing dashboard"),
        None => writeln!(writer, "latest:            (none)").expect("writing dashboard"),
    }
    match goal {
        Some(goal) => {
            let budget = goal.token_budget.map_or_else(String::new, |budget| {
                format!(", budget {budget}")
            });
            writeln!(
                writer,
                "goal:              {} — \"{}\" ({} tokens used{budget})",
                goal.lifecycle, goal.objective, goal.tokens_used
            )
            .expect("writing dashboard");
        }
        None => writeln!(writer, "goal:              none").expect("writing dashboard"),
    }
    writeln!(writer, "tools ({}):        {}", tools.len(), tools.join(" "))
        .expect("writing dashboard");
}

fn write_dashboard_json<W: Write>(
    writer: &mut W,
    cwd: &Path,
    session_dir: &Path,
    sessions: usize,
    latest: Option<(&Path, &(String, usize))>,
    goal: Option<&GoalSummary>,
    tools: &[&str],
) -> Result<()> {
    let report = serde_json::json!({
        "command": "dashboard",
        "cwd": cwd,
        "sessionRoot": session_dir,
        "sessionCount": sessions,
        "latestSession": latest.map(|(path, (timestamp, messages))| serde_json::json!({
            "path": path,
            "timestamp": timestamp,
            "messages": messages,
        })),
        "goal": goal.map(|goal| serde_json::json!({
            "lifecycle": goal.lifecycle,
            "objective": goal.objective,
            "tokensUsed": goal.tokens_used,
            "tokenBudget": goal.token_budget,
        })),
        "tools": tools,
    });
    writeln!(writer, "{}", serde_json::to_string(&report)?)?;
    Ok(())
}

/// Resolve the agent configuration directory (parent of models.json).
fn agent_dir() -> Option<PathBuf> {
    models_json_path().and_then(|path| path.parent().map(Path::to_path_buf))
}

fn resolve_cwd(cli: &crate::Cli) -> Result<PathBuf> {
    let cwd = cli.cwd.clone().unwrap_or(std::env::current_dir()?);
    cwd.canonicalize()
        .with_context(|| format!("resolving working directory {}", cwd.display()))
}

fn trust_override(cli: &crate::Cli) -> Option<bool> {
    if cli.approve {
        Some(true)
    } else if cli.no_approve {
        Some(false)
    } else {
        None
    }
}

/// Load the settings graph and return the configured `sessionDir`, mirroring
/// the `sessions` subcommand's resolution. Errors are left to the caller so
/// diagnostics can degrade to defaults.
fn load_settings_session_dir(cwd: &Path, cli: &crate::Cli) -> Result<Option<PathBuf>> {
    let mut options = pi_coding::ResourceManagerOptions::new(cwd);
    options.headless = true;
    options.project_trust_override = trust_override(cli);
    let resources =
        pi_coding::ResourceManager::new(options).context("loading settings and resources")?;
    Ok(resources.snapshot().settings.session_dir.clone())
}

/// Probe session-directory writability by creating and removing a probe file.
/// The directory is created if missing, matching what a run would do.
fn probe_writable(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let probe = dir.join(format!(".rpi-doctor-write-probe-{}", std::process::id()));
    let file = std::fs::File::create(&probe)
        .with_context(|| format!("creating probe file in {}", dir.display()))?;
    drop(file);
    std::fs::remove_file(&probe)
        .with_context(|| format!("removing probe file {}", probe.display()))?;
    Ok(())
}

/// Probe `git` availability, returning its version line.
fn git_version() -> Result<String> {
    let output = std::process::Command::new("git")
        .arg("--version")
        .output()
        .context("spawning git")?;
    if !output.status.success() {
        bail!("git --version exited with {}", output.status);
    }
    let version = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if version.is_empty() {
        bail!("git --version produced no output");
    }
    Ok(version)
}

/// Read-only `models.json` parse: size-bound read, comment stripping, and JSON
/// validation, counting providers and explicitly listed models. The global
/// snapshot is never touched, so this is safe to run anywhere.
fn dry_parse_models_json(path: &Path) -> Result<(usize, usize)> {
    let metadata = std::fs::metadata(path)
        .with_context(|| format!("reading metadata for {}", path.display()))?;
    if !metadata.is_file() {
        bail!("path is not a file: {}", path.display());
    }
    if metadata.len() > MAX_CONFIG_FILE_BYTES {
        bail!("file exceeds {MAX_CONFIG_FILE_BYTES} bytes");
    }
    let content =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    if content.trim().is_empty() {
        return Ok((0, 0));
    }
    let value: serde_json::Value = serde_json::from_str(&strip_comments(&content))
        .with_context(|| format!("Failed to parse models.json\nFile: {}", path.display()))?;
    let providers = value
        .get("providers")
        .and_then(serde_json::Value::as_object)
        .map_or(0, |map| map.len());
    let models = value
        .get("providers")
        .and_then(serde_json::Value::as_object)
        .map_or(0, |map| {
            map.values()
                .filter_map(|provider| {
                    provider
                        .get("models")
                        .and_then(serde_json::Value::as_array)
                })
                .map(Vec::len)
                .sum()
        });
    Ok((providers, models))
}

/// Strip `//` and `/* */` comments outside string literals, mirroring the
/// loader's `models_config::strip_json_comments` semantics so the read-only
/// doctor parse accepts exactly what the real loader accepts.
fn strip_comments(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut output = Vec::with_capacity(input.len());
    let mut index = 0;
    let mut in_string = false;
    let mut escaped = false;
    while index < bytes.len() {
        let current = bytes[index];
        if in_string {
            output.push(current);
            if escaped {
                escaped = false;
            } else if current == b'\\' {
                escaped = true;
            } else if current == b'"' {
                in_string = false;
            }
            index += 1;
            continue;
        }
        if current == b'"' {
            in_string = true;
            output.push(current);
            index += 1;
            continue;
        }
        if current == b'/' && bytes.get(index + 1) == Some(&b'/') {
            index += 2;
            while index < bytes.len() && bytes[index] != b'\n' {
                index += 1;
            }
            continue;
        }
        if current == b'/' && bytes.get(index + 1) == Some(&b'*') {
            index += 2;
            while index + 1 < bytes.len() && !(bytes[index] == b'*' && bytes[index + 1] == b'/') {
                index += 1;
            }
            index = (index + 2).min(bytes.len());
            continue;
        }
        output.push(current);
        index += 1;
    }
    String::from_utf8(output).unwrap_or_else(|_| input.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse a `rpi <subcommand>` CLI with the given extra args (e.g.
    /// `--session-dir`) and validate it, mirroring the dispatch entry point.
    fn parse_subcommand(subcommand: &str, extra: &[&str], json: bool) -> crate::Cli {
        let mut args = vec!["rpi".to_owned(), subcommand.to_owned()];
        args.extend(extra.iter().map(|arg| (*arg).to_owned()));
        if json {
            args.push("--json".to_owned());
        }
        let cli = crate::Cli::try_parse_from(args).expect("parse subcommand");
        cli.validate().expect("validate subcommand");
        cli
    }

    /// Render a doctor/dashboard run into a buffer through the same functions
    /// the CLI entry point uses, so tests assert on the exact bytes.
    fn render(cli: &crate::Cli, json: bool) -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        match &cli.command {
            Some(crate::Command::Doctor { .. }) => doctor_into(cli, json, &mut buf)?,
            Some(crate::Command::Dashboard { .. }) => dashboard_into(cli, json, &mut buf)?,
            other => panic!("unexpected command {other:?}"),
        }
        Ok(buf)
    }

    /// Pin a file's last-modified time (stable since Rust 1.75).
    fn set_mtime(path: &Path, time: std::time::SystemTime) {
        let file = std::fs::File::options()
            .write(true)
            .open(path)
            .expect("open session for mtime");
        file.set_times(std::fs::FileTimes::new().set_modified(time))
            .expect("set session mtime");
    }

    #[test]
    fn doctor_emits_version_and_all_check_sections() {
        let session_root = tempfile::tempdir().expect("session dir");
        let cli = parse_subcommand(
            "doctor",
            &["--session-dir", session_root.path().to_str().expect("utf-8")],
            false,
        );
        let output = render(&cli, false).expect("doctor");
        let text = String::from_utf8(output).expect("utf-8");
        for section in [
            "rpi doctor",
            "rpi version:",
            "agent dir",
            "models.json",
            "auth.json",
            "settings",
            "session dir",
            "git",
            "providers",
        ] {
            assert!(text.contains(section), "missing section {section:?} in:\n{text}");
        }
        assert!(text.contains("PASS"), "expected PASS lines in:\n{text}");
        assert!(
            text.contains(session_root.path().display().to_string().as_str()),
            "session dir check must report the resolved path:\n{text}"
        );
    }

    #[test]
    fn doctor_json_report_is_parseable_and_has_expected_shape() {
        let session_root = tempfile::tempdir().expect("session dir");
        let cli = parse_subcommand(
            "doctor",
            &["--session-dir", session_root.path().to_str().expect("utf-8")],
            true,
        );
        let output = render(&cli, true).expect("doctor json");
        let report: serde_json::Value = serde_json::from_slice(&output).expect("doctor json parses");
        assert_eq!(report["command"], "doctor");
        assert!(report["version"].is_string());
        let checks = report["checks"].as_array().expect("checks array");
        assert!(!checks.is_empty());
        for check in checks {
            assert!(check["name"].is_string());
            assert!(check["status"].is_string());
            assert!(check["detail"].is_string());
        }
        assert!(report["failed"].is_u64());
    }

    #[test]
    fn doctor_never_leaks_auth_secrets_in_text_or_json() {
        let dir = tempfile::tempdir().expect("tempdir");
        let auth = dir.path().join("auth.json");
        std::fs::write(
            &auth,
            r#"{"secret-provider":{"type":"api_key","key":"SUPER-SECRET-TOKEN-ABC"}}"#,
        )
        .expect("write auth fixture");
        for json in [false, true] {
            let check = auth_json_check(Some(&auth));
            let mut buf = Vec::new();
            if json {
                write_doctor_json(&mut buf, std::slice::from_ref(&check)).expect("doctor json");
            } else {
                write_doctor_text(&mut buf, std::slice::from_ref(&check));
            }
            let text = String::from_utf8(buf).expect("utf-8");
            assert!(
                !text.contains("SUPER-SECRET-TOKEN-ABC"),
                "secret leaked in doctor output:\n{text}"
            );
            assert!(
                !text.contains("api_key"),
                "credential shape leaked in doctor output:\n{text}"
            );
            // The provider name and path are the only auth.json facts.
            assert!(
                text.contains("secret-provider"),
                "provider name should be reported:\n{text}"
            );
        }
    }

    #[test]
    fn doctor_malformed_models_json_reports_fail_without_content() {
        let dir = tempfile::tempdir().expect("tempdir");
        let models = dir.path().join("models.json");
        std::fs::write(&models, "{ must-not-leak-model-content").expect("write malformed models");
        let check = models_json_check(Some(&models));
        assert_eq!(check.status, Status::Fail);
        let mut buf = Vec::new();
        write_doctor_text(&mut buf, std::slice::from_ref(&check));
        let text = String::from_utf8(buf).expect("utf-8");
        assert!(text.contains("FAIL"), "expected FAIL for malformed models.json:\n{text}");
        assert!(text.contains("models.json"), "expected models.json check:\n{text}");
        assert!(
            !text.contains("must-not-leak-model-content"),
            "models.json content leaked:\n{text}"
        );
    }

    #[test]
    fn setup_noninteractive_prints_just_the_two_paths() {
        let agent = tempfile::tempdir().expect("agent dir");
        let mut buf = Vec::new();
        setup_into(false, Some(agent.path()), &mut buf).expect("setup");
        let text = String::from_utf8(buf).expect("utf-8");
        let lines = text.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 2, "expected exactly two path lines, got:\n{text}");
        assert_eq!(lines[0], agent.path().join("models.json").display().to_string());
        assert_eq!(lines[1], agent.path().join("auth.json").display().to_string());
    }

    #[test]
    fn setup_json_reports_paths_and_examples_without_secrets() {
        let agent = tempfile::tempdir().expect("agent dir");
        let mut buf = Vec::new();
        setup_into(true, Some(agent.path()), &mut buf).expect("setup json");
        let report: serde_json::Value = serde_json::from_slice(&buf).expect("setup parses");
        assert_eq!(report["command"], "setup");
        assert_eq!(
            report["modelsJson"],
            serde_json::Value::String(agent.path().join("models.json").display().to_string())
        );
        assert_eq!(
            report["authJson"],
            serde_json::Value::String(agent.path().join("auth.json").display().to_string())
        );
        let examples = &report["examples"];
        assert!(examples["modelsJson"].as_str().is_some_and(|s| s.contains("providers")));
        assert!(examples["authJson"].as_str().is_some_and(|s| s.contains("api_key")));
    }

    #[test]
    fn setup_unresolved_agent_dir_reports_guidance() {
        let mut buf = Vec::new();
        setup_into(false, None, &mut buf).expect("setup without agent dir");
        let text = String::from_utf8(buf).expect("utf-8");
        assert!(
            text.contains("agent directory: unavailable"),
            "missing guidance for unresolved agent dir:\n{text}"
        );
    }

    #[test]
    fn dashboard_empty_root_reports_zero_sessions() {
        let root = tempfile::tempdir().expect("session root");
        let cli = parse_subcommand(
            "dashboard",
            &["--session-dir", root.path().to_str().expect("utf-8")],
            false,
        );
        let output = render(&cli, false).expect("dashboard");
        let text = String::from_utf8(output).expect("utf-8");
        for section in [
            "rpi dashboard",
            "working directory:",
            "session root:",
            "sessions:          0",
            "latest:            (none)",
            "goal:              none",
            "tools (",
        ] {
            assert!(text.contains(section), "missing {section:?} in:\n{text}");
        }
    }

    #[test]
    fn dashboard_redacts_credential_shaped_goal_objectives() {
        let root = tempfile::tempdir().expect("session root");
        let credential = ["gh", "p_", "ABCDEFGHIJKLMNOPQRSTUVWXYZ123456"].concat();
        let session = root.path().join("2026-01-02T00-00-00Z_token.jsonl");
        let fixture = concat!(
            "{\"type\":\"session\",\"version\":3,\"id\":\"token\",\"timestamp\":\"2026-01-02T00:00:00Z\",\"cwd\":\"/tmp\"}\n",
            "{\"type\":\"custom\",\"id\":\"g1\",\"parentId\":null,\"timestamp\":\"2026-01-02T00:00:01Z\",\"customType\":\"pi.goal.event\",\"data\":{\"version\":1,\"event\":{\"revision\":1,\"timestamp\":\"2026-01-02T00:00:01Z\",\"kind\":{\"type\":\"created\"},\"goal\":{\"id\":\"goal-1\",\"objective\":\"deploy with <credential>\",\"lifecycle\":\"active\",\"createdAt\":\"2026-01-02T00:00:01Z\",\"updatedAt\":\"2026-01-02T00:00:01Z\",\"usage\":{\"tokensUsed\":0,\"activeTimeSeconds\":0}}}}}\n",
        )
        .replace("<credential>", &credential);
        std::fs::write(&session, fixture).expect("write session with token-shaped goal");

        let cli = parse_subcommand(
            "dashboard",
            &["--session-dir", root.path().to_str().expect("utf-8")],
            false,
        );
        let text = String::from_utf8(render(&cli, false).expect("dashboard")).expect("utf-8");
        assert!(
            !text.contains(&credential),
            "dashboard text must not leak the token:\n{text}"
        );
        assert!(
            text.contains("deploy with [REDACTED]"),
            "dashboard must redact the token in the goal line:\n{text}"
        );

        let cli_json = parse_subcommand(
            "dashboard",
            &["--session-dir", root.path().to_str().expect("utf-8")],
            true,
        );
        let report: serde_json::Value =
            serde_json::from_slice(&render(&cli_json, true).expect("dashboard json"))
                .expect("dashboard parses");
        assert_eq!(
            report["goal"]["objective"],
            "deploy with [REDACTED]",
            "dashboard JSON must redact the goal objective"
        );
    }

    #[test]
    fn dashboard_counts_sessions_and_reads_goal_state() {
        let root = tempfile::tempdir().expect("session root");
        let older = root.path().join("2026-01-01T00-00-00Z_older.jsonl");
        let newer = root.path().join("2026-01-02T00-00-00Z_newer.jsonl");
        std::fs::write(
            &older,
            concat!(
                "{\"type\":\"session\",\"version\":3,\"id\":\"older\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"cwd\":\"/tmp\"}\n",
            ),
        )
        .expect("write older session");
        std::fs::write(
            &newer,
            concat!(
                "{\"type\":\"session\",\"version\":3,\"id\":\"newer\",\"timestamp\":\"2026-01-02T00:00:00Z\",\"cwd\":\"/tmp\"}\n",
                "{\"type\":\"message\",\"id\":\"m1\",\"parentId\":null,\"timestamp\":\"2026-01-02T00:00:01Z\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"hi\"}],\"timestamp\":0}}\n",
                // Created snapshots must carry zero usage and created_at == updated_at.
                "{\"type\":\"custom\",\"id\":\"g1\",\"parentId\":\"m1\",\"timestamp\":\"2026-01-02T00:00:02Z\",\"customType\":\"pi.goal.event\",\"data\":{\"version\":1,\"event\":{\"revision\":1,\"timestamp\":\"2026-01-02T00:00:02Z\",\"kind\":{\"type\":\"created\"},\"goal\":{\"id\":\"goal-1\",\"objective\":\"Ship the release\",\"lifecycle\":\"active\",\"createdAt\":\"2026-01-02T00:00:02Z\",\"updatedAt\":\"2026-01-02T00:00:02Z\",\"usage\":{\"tokensUsed\":0,\"activeTimeSeconds\":0}}}}}\n",
                // Usage accrues through a usage_updated transition.
                "{\"type\":\"custom\",\"id\":\"g2\",\"parentId\":\"g1\",\"timestamp\":\"2026-01-02T00:00:03Z\",\"customType\":\"pi.goal.event\",\"data\":{\"version\":1,\"event\":{\"revision\":2,\"timestamp\":\"2026-01-02T00:00:03Z\",\"kind\":{\"type\":\"usage_updated\",\"delta\":{\"tokens\":1200,\"activeTimeSeconds\":60}},\"goal\":{\"id\":\"goal-1\",\"objective\":\"Ship the release\",\"lifecycle\":\"active\",\"createdAt\":\"2026-01-02T00:00:02Z\",\"updatedAt\":\"2026-01-02T00:00:03Z\",\"usage\":{\"tokensUsed\":1200,\"activeTimeSeconds\":60}}}}}\n",
            ),
        )
        .expect("write newer session with goal");
        // Pin mtimes so "latest" is deterministic regardless of write order
        // or filesystem timestamp granularity.
        let older_time =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let newer_time = older_time + std::time::Duration::from_secs(86_400);
        set_mtime(&older, older_time);
        set_mtime(&newer, newer_time);

        let cli = parse_subcommand(
            "dashboard",
            &["--session-dir", root.path().to_str().expect("utf-8")],
            false,
        );
        let output = render(&cli, false).expect("dashboard");
        let text = String::from_utf8(output).expect("utf-8");
        assert!(text.contains("sessions:          2"), "count in:\n{text}");
        assert!(text.contains("newer"), "latest session should be newer:\n{text}");
        assert!(
            text.contains("goal:              active — \"Ship the release\" (1200 tokens used)"),
            "goal line in:\n{text}"
        );

        let cli_json = parse_subcommand(
            "dashboard",
            &["--session-dir", root.path().to_str().expect("utf-8")],
            true,
        );
        let json_output = render(&cli_json, true).expect("dashboard json");
        let report: serde_json::Value =
            serde_json::from_slice(&json_output).expect("dashboard parses");
        assert_eq!(report["command"], "dashboard");
        assert_eq!(report["sessionCount"], 2);
        assert_eq!(report["latestSession"]["timestamp"], "2026-01-02T00:00:00Z");
        assert_eq!(report["latestSession"]["messages"], 1);
        assert_eq!(report["goal"]["lifecycle"], "active");
        assert_eq!(report["goal"]["objective"], "Ship the release");
        assert_eq!(report["goal"]["tokensUsed"], 1200);
        let tools = report["tools"].as_array().expect("tools array");
        assert!(tools.iter().any(|tool| tool == "read"));
    }

    #[test]
    fn dry_parse_models_json_counts_providers_and_models_with_comments() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("models.json");
        std::fs::write(
            &path,
            r#"{
  // providers are keyed by id
  "providers": {
    "alpha": { "baseUrl": "https://alpha.test/v1", "models": [{ "id": "a1" }, { "id": "a2" }] },
    "beta": { "baseUrl": "https://beta.test/v1", "models": [{ "id": "b1" }] }
  }
}"#,
        )
        .expect("write models fixture");
        let (providers, models) = dry_parse_models_json(&path).expect("dry parse");
        assert_eq!(providers, 2);
        assert_eq!(models, 3);
        let check = models_json_check(Some(&path));
        assert_eq!(check.status, Status::Pass);
        assert!(check.detail.contains("2 providers, 3 models"), "detail: {}", check.detail);
    }

    #[test]
    fn truncate_shortens_long_objectives() {
        assert_eq!(truncate("short", 80), "short");
        let long = "x".repeat(200);
        let truncated = truncate(&long, 80);
        assert_eq!(truncated.chars().count(), 81, "80 chars plus ellipsis");
        assert!(truncated.ends_with('…'));
    }

    #[test]
    fn subcommands_parse_and_validate() {
        for args in [
            ["rpi", "doctor"].as_slice(),
            ["rpi", "doctor", "--json"].as_slice(),
            ["rpi", "setup"].as_slice(),
            ["rpi", "setup", "--json"].as_slice(),
            ["rpi", "dashboard"].as_slice(),
            ["rpi", "dashboard", "--json"].as_slice(),
        ] {
            let cli = crate::Cli::try_parse_from(args).expect("parse subcommand");
            cli.validate().expect("validate subcommand");
        }
        assert!(crate::Cli::try_parse_from(["rpi", "doctor", "--json", "extra"]).is_err());
    }
}
