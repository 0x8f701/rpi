//! Session setup, resume, and print-mode execution.
//!
//! This wires the parsed CLI flags into the `pi_coding::Session` facade,
//! mirroring the Go `main()`: resume-path resolution, model restoration from
//! a resumed branch, API-key gating, reasoning-level priority, session
//! recording, and print-mode streaming.

use std::collections::HashSet;
use std::fs;
use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, OnceLock, atomic::{AtomicBool, Ordering}};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};

use crate::args::Cli;
use crate::commands::{resolve_model_spec, resolve_resume_for_startup};
use crate::extension_ui::ExtensionUiAdapter;
use crate::models_config::ModelRequestAuth;
use crate::session_run_blueprint::RunSessionBlueprint;
use crate::output::{parse_thinking_level, thinking_level_str, warn_line};
use pi_coding::{
    Application, ApplicationRuntimeFactory, BranchContext, DEFAULT_COMPACTION_SETTINGS,
    ExtensionMode, ResourceManagerOptions, SessionOptions,
};

static OFFLINE: AtomicBool = AtomicBool::new(false);

pub fn set_offline(offline: bool) {
    OFFLINE.store(offline, Ordering::Release);
}

#[must_use]
pub fn offline() -> bool {
    OFFLINE.load(Ordering::Acquire)
        || std::env::var("PI_OFFLINE").is_ok_and(|value| {
            matches!(value.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes")
        })
}

/// The fully-resolved live application and its mode-specific extension UI adapter.
pub struct RunSession {
    pub application: Application,
    pub extension_ui: Option<ExtensionUiAdapter>,
    pub model: pi_ai::Model,
    pub scoped_models: Option<Vec<pi_ai::Model>>,
    /// Non-fatal startup warnings (settings deprecations, resource diagnostics)
    /// collected for interactive TUI display. Empty for non-interactive modes
    /// where warnings are emitted directly to stderr instead.
    pub startup_warnings: Vec<String>,
    /// Cloneable factory for building additional session runtimes for the Web
    /// control plane (`session_run::build_session` policy from a sanitized
    /// clone of the startup CLI). Consumed by `modes::listen`.
    pub spawner: RunSessionSpawner,
}

/// Builds manager-owned session runtimes for the Web/listen control plane.
///
/// Children are built with the SAME production policy as startup
/// (`RunSessionBlueprint` + `ApplicationRuntimeFactory`) from a sanitized
/// clone of the startup [`Cli`], so auth resolver, orchestration, goal,
/// resources, and extension approval isolation are preserved — without
/// re-running process-global catalog loads or clearing runtime API keys.
///
/// Every child gets its own `Application` and a NON-INTERACTIVE extension UI
/// host (`extension_ui: None` in the blueprint): secondary Web-only sessions
/// must never share the primary TUI's approval slot, and approval-required
/// tools fail closed instead of hanging or routing to the wrong session.
#[derive(Clone)]
pub struct RunSessionSpawner {
    cli: Cli,
    session_dir: PathBuf,
}

impl RunSessionSpawner {
    pub(crate) fn from_startup(cli: &Cli, session_dir: PathBuf) -> Self {
        Self {
            cli: sanitize_cli_for_children(cli),
            session_dir,
        }
    }

    /// Open (resume) the persisted session at `path` as an independent
    /// runtime. The target cwd, model, and history come from the recorded
    /// session file (backend-authoritative), never from a frontend cache.
    pub(crate) async fn open_resumed(
        &self,
        source: &Application,
        path: &Path,
    ) -> Result<crate::modes::session_runtime_manager::SessionSpawnResult> {
        let prepared = pi_coding::PreparedSessionResume::prepare_path(path)
            .with_context(|| format!("opening session {}", path.display()))?;
        let recorded_cwd = prepared.target_cwd();
        let mut target_cwd = source.session().cwd().to_path_buf();
        if !recorded_cwd.as_os_str().is_empty() && recorded_cwd.exists() {
            target_cwd = recorded_cwd.canonicalize().with_context(|| {
                format!("resolving resumed working directory {}", recorded_cwd.display())
            })?;
        }
        let blueprint = self.child_blueprint()?;
        let options = child_session_options(source, &target_cwd);
        let candidate = blueprint
            .build_runtime_candidate(target_cwd.clone(), options, Some(prepared))
            .await
            .context("building resumed session runtime")?;
        // The child blueprint carries the session dir, so the built session
        // is already configured (ApplicationRuntimeCandidate.session is
        // crate-private by design).
        let application = Application::from_runtime_candidate(candidate).await?;
        application.attach_runtime_factory(Arc::new(blueprint.clone()))?;
        application.prepare_resumed_goal(false)?;
        let (session_id, session_file) = application
            .session()
            .recorder_info()
            .ok_or_else(|| anyhow!("resumed session has no recorder"))?;
        setup_child_workflows(&application, &target_cwd, &session_id).await?;
        Ok(crate::modes::session_runtime_manager::SessionSpawnResult {
            session_id,
            session_file: Some(session_file),
            application,
            extension_ui: ExtensionUiAdapter::default(),
        })
    }

    /// Start a brand-new recorded session as an independent runtime. The
    /// child inherits the source's model/thinking/auth resolver and its own
    /// session-scoped workflow storage.
    pub(crate) async fn new_session(
        &self,
        source: &Application,
    ) -> Result<crate::modes::session_runtime_manager::SessionSpawnResult> {
        let cwd = source.session().cwd().to_path_buf();
        let blueprint = self.child_blueprint()?;
        let options = child_session_options(source, &cwd);
        let candidate = blueprint
            .build_runtime_candidate(cwd.clone(), options, None)
            .await
            .context("building fresh session runtime")?;
        let application = Application::from_runtime_candidate(candidate).await?;
        application.attach_runtime_factory(Arc::new(blueprint.clone()))?;
        let (session_id, session_file) = application
            .session()
            .recorder_info()
            .ok_or_else(|| anyhow!("new session has no recorder"))?;
        setup_child_workflows(&application, &cwd, &session_id).await?;
        Ok(crate::modes::session_runtime_manager::SessionSpawnResult {
            session_id,
            session_file: Some(session_file),
            application,
            extension_ui: ExtensionUiAdapter::default(),
        })
    }

    /// Fork the source session at `entry_id` into an independent runtime.
    /// Mirrors the in-place `/fork` semantics: a branched session file is
    /// created under the source's parent entry and the restored conversation
    /// comes from that branch (backend recorder), not the frontend.
    pub(crate) async fn fork_session(
        &self,
        source: &Application,
        entry_id: &str,
    ) -> Result<crate::modes::session_runtime_manager::SessionSpawnResult> {
        let (source_path, _) = source
            .session()
            .recorder_info()
            .ok_or_else(|| anyhow!("source session has no recorder"))?;
        let tree = pi_coding::load_session_tree(Path::new(&source_path))
            .with_context(|| format!("loading session {}", Path::new(&source_path).display()))?;
        let selected = tree
            .entries
            .iter()
            .find(|entry| entry.id == entry_id)
            .ok_or_else(|| anyhow!("invalid entry id for fork: {entry_id}"))?;
        if !matches!(selected.message, Some(pi_ai::Message::User(_))) {
            bail!("invalid entry id for fork: {entry_id} (entry is not a user message)");
        }
        let Some(parent_id) = selected.parent_id.as_deref() else {
            // Root entry: no branch to create; the fork starts fresh,
            // mirroring the in-place fork's no-parent path.
            return self.new_session(source).await;
        };
        let recorder = pi_coding::create_branched_session_in(
            &source_path,
            parent_id,
            Some(&self.session_dir),
        )?;
        recorder.persist_now()?;
        let branched = recorder.path();
        self.open_resumed(source, &branched).await
    }

    /// Clone the source session's active leaf into an independent runtime
    /// (mirrors in-place `/clone`).
    pub(crate) async fn clone_session(
        &self,
        source: &Application,
    ) -> Result<crate::modes::session_runtime_manager::SessionSpawnResult> {
        let (source_path, _) = source
            .session()
            .recorder_info()
            .ok_or_else(|| anyhow!("source session has no recorder"))?;
        let leaf_id = source
            .session()
            .session_tree()?
            .active_leaf_id
            .ok_or_else(|| anyhow!("cannot clone session: no current entry selected"))?;
        let recorder =
            pi_coding::create_branched_session_in(&source_path, &leaf_id, Some(&self.session_dir))?;
        recorder.persist_now()?;
        let branched = recorder.path();
        self.open_resumed(source, &branched).await
    }

    /// Rebuild the child blueprint from the sanitized CLI. `extension_ui` is
    /// deliberately None so the child uses a per-session non-interactive
    /// (fail-closed) approval host instead of the primary TUI adapter.
    fn child_blueprint(&self) -> Result<RunSessionBlueprint> {
        let (_, resource_options) = startup_resource_options(&self.cli, true)?;
        let mut blueprint = RunSessionBlueprint::from_cli(&self.cli, resource_options, None);
        blueprint.set_session_dir(self.session_dir.clone());
        Ok(blueprint)
    }
}

impl crate::modes::session_runtime_manager::SessionSpawner for RunSessionSpawner {
    fn spawn(
        &self,
        request: crate::modes::session_runtime_manager::SessionSpawnRequest,
    ) -> Pin<Box<dyn Future<Output = Result<crate::modes::session_runtime_manager::SessionSpawnResult>> + Send>>
    {
        let this = self.clone();
        Box::pin(async move {
            match &request.kind {
                crate::modes::session_runtime_manager::SessionSpawnKind::Open { resume_path } => {
                    this.open_resumed(&request.source, resume_path).await
                }
                crate::modes::session_runtime_manager::SessionSpawnKind::Fresh => {
                    this.new_session(&request.source).await
                }
                crate::modes::session_runtime_manager::SessionSpawnKind::Fork { entry_id } => {
                    this.fork_session(&request.source, entry_id).await
                }
                crate::modes::session_runtime_manager::SessionSpawnKind::Clone => {
                    this.clone_session(&request.source).await
                }
            }
        })
    }
}

/// Child `SessionOptions` derived from the source runtime's live snapshot,
/// mirroring the cross-directory switch construction exactly.
fn child_session_options(source: &Application, cwd: &Path) -> SessionOptions {
    let options = source.session().child_session_options_snapshot();
    SessionOptions {
        model: options.model,
        cwd: cwd.to_path_buf(),
        system_prompt: String::new(),
        thinking_level: options.thinking_level,
        api_key: options.api_key,
        compaction: source.session().compaction_settings(),
        stream_options: options.stream_options,
        tools: None,
        before_tool_call: None,
        after_tool_call: None,
        stream_fn: Some(options.stream_fn),
        auth_resolver: options.auth_resolver,
    }
}

/// Attach session-scoped workflow storage for a spawned child: same policy as
/// startup (`setup_workflows` with roots namespaced by the child's session
/// identity), so per-session Workflow RPC state is isolated.
async fn setup_child_workflows(
    application: &Application,
    cwd: &Path,
    session_id: &str,
) -> Result<()> {
    let agent_dir = pi_coding::agent_dir_path();
    let (store_root, worktree_root) = workflow_storage_roots(cwd, &agent_dir, session_id);
    application
        .setup_workflows(cwd.to_path_buf(), store_root, worktree_root)
        .await
        .context("binding workflow storage for spawned session")?;
    Ok(())
}

/// The child CLI inherits every startup policy (tools, extensions, trust,
/// approval mode, model scoping, profiles) but drops per-run baggage: no
/// nested listener, no resume/open/fork selection, no initial prompts, and
/// recording is FORCED so every manager-owned runtime has a durable session
/// id (the routing registry must never key an empty id).
fn sanitize_cli_for_children(cli: &Cli) -> Cli {
    let mut clone = cli.clone();
    clone.listen = None;
    clone.listen_token_file = None;
    clone.listen_allow_insecure_remote = false;
    clone.prompt = Vec::new();
    clone.resume = None;
    clone.session = None;
    clone.fork = None;
    clone.continue_latest = false;
    clone.session_id = None;
    clone.no_session = false;
    clone.name = None;
    clone.export = None;
    clone.output = None;
    clone.jsonl = false;
    clone.list_models = None;
    clone.print = false;
    clone
}

pub(crate) fn extension_mode(cli: &Cli) -> ExtensionMode {
    match cli.mode {
        Some(crate::args::Mode::Json) => ExtensionMode::Json,
        Some(crate::args::Mode::Rpc) => ExtensionMode::Rpc,
        Some(crate::args::Mode::Text) => ExtensionMode::Print,
        None if cli.is_print_mode() => ExtensionMode::Print,
        None => ExtensionMode::Tui,
    }
}

async fn authenticated_model(
    model: pi_ai::Model,
    explicit_key: Option<&str>,
) -> Result<Option<(pi_ai::Model, ModelRequestAuth)>> {
    if explicit_key.is_none() && !crate::models_config::has_configured_auth(&model) {
        return Ok(None);
    }
    let auth = crate::models_config::resolve_model_request_auth_async(&model, explicit_key, None).await?;
    Ok(crate::models_config::model_is_available_for_request_auth(&model, &auth)
        .then_some((model, auth)))
}

async fn first_authenticated_model(
    explicit_key: Option<&str>,
) -> Result<Option<(pi_ai::Model, ModelRequestAuth)>> {
    let mut providers = pi_ai::get_providers();
    providers.sort();
    providers.retain(|provider| provider != "faux");
    let mut candidates = Vec::new();
    for provider in &providers {
        if let Some(id) = pi_coding::default_model_per_provider(provider)
            && let Some(model) = pi_ai::get_model(provider, id)
        {
            candidates.push(model);
        }
    }
    let mut seen = candidates
        .iter()
        .map(|model| (model.provider.clone(), model.id.clone()))
        .collect::<HashSet<_>>();
    for provider in providers {
        let mut models = pi_ai::get_models(&provider);
        models.sort_by(|left, right| left.id.cmp(&right.id));
        for model in models {
            if seen.insert((model.provider.clone(), model.id.clone())) {
                candidates.push(model);
            }
        }
    }
    for model in candidates {
        if let Some(selected) = authenticated_model(model, explicit_key).await? {
            return Ok(Some(selected));
        }
    }
    Ok(None)
}

/// Resolve the initial model for a session: explicit CLI spec, an
/// authenticated resumed model, the authenticated settings default, then the
/// first authenticated model. Returns `(model, api_key, parsed_think)` where
/// `parsed_think` is the reasoning-level suffix extracted from an explicit
/// model spec (empty when none was present).
///
/// Shared by [`build_session`] and the ACP agent mode so both entrypoints
/// resolve models with identical precedence.
pub(crate) async fn resolve_initial_model(
    cli: &Cli,
    settings: &pi_coding::Settings,
    resume_ctx: Option<&BranchContext>,
) -> Result<(pi_ai::Model, String, String)> {
    let explicit_model = cli.model.as_deref().filter(|spec| !spec.is_empty());
    let explicit_key = cli.api_key.as_deref();
    let mut parsed_think = String::new();
    let mut selected: Option<(pi_ai::Model, ModelRequestAuth)> = None;
    let explicit_model_spec = explicit_model.map(|spec| {
        let Some(provider) = cli.provider.as_deref() else {
            return spec.to_owned();
        };
        let prefix = format!("{provider}/");
        if spec.get(..prefix.len()).is_some_and(|value| value.eq_ignore_ascii_case(&prefix)) {
            spec.to_owned()
        } else {
            format!("{provider}/{spec}")
        }
    });
    if let Some(spec) = explicit_model_spec.as_deref() {
        let (model, level) = resolve_model_spec(spec)?;
        parsed_think = level;
        selected = authenticated_model(model, explicit_key).await?;
        if selected.is_none() {
            bail!("Model {spec:?} is not available for the resolved credential");
        }
    } else if let Some(ctx) = resume_ctx
        && let (Some(provider), Some(id)) = (ctx.provider.as_ref(), ctx.model_id.as_ref())
    {
        match pi_ai::get_model(provider, id) {
            Some(model) => match authenticated_model(model, explicit_key).await? {
                Some(resolved) => selected = Some(resolved),
                None => warn_line(&format!(
                    "Warning: Could not restore model {provider}/{id} (no auth configured)"
                )),
            },
            None => warn_line(&format!("Warning: Could not restore model {provider}/{id}")),
        }
    }
    if selected.is_none()
        && let (Some(provider), Some(id)) = (
            settings
                .default_provider
                .as_deref()
                .filter(|value| !value.is_empty()),
            settings
                .default_model
                .as_deref()
                .filter(|value| !value.is_empty()),
        )
        && let Some(model) = pi_ai::get_model(provider, id)
    {
        selected = authenticated_model(model, explicit_key).await?;
    }
    if selected.is_none() {
        selected = first_authenticated_model(explicit_key).await?;
    }
    let (model, auth) = selected.ok_or_else(|| {
        anyhow!(
            "No authenticated models available. Configure auth.json, models.json, a provider API-key environment variable, or pass --model with --api-key."
        )
    })?;
    if let Some(key) = explicit_key.filter(|key| !key.trim().is_empty()) {
        crate::models_config::set_runtime_api_key(&model.provider, key);
    }
    Ok((model, auth.api_key, parsed_think))
}


pub(crate) fn resolve_initial_thinking_level(
    cli_level: Option<&str>,
    model_level: &str,
    resume: Option<&BranchContext>,
    resume_has_thinking_entry: bool,
    settings_level: Option<&str>,
) -> pi_agent::ThinkingLevel {
    let level = cli_level
        .filter(|value| !value.is_empty())
        .or_else(|| (!model_level.is_empty()).then_some(model_level))
        .or_else(|| {
            resume
                .filter(|_| resume_has_thinking_entry)
                .map(|context| context.thinking_level.as_str())
        })
        .or_else(|| settings_level.filter(|value| !value.is_empty()))
        .unwrap_or_default();
    parse_thinking_level(level)
}

pub(crate) async fn resolve_model_scope(patterns: &[String]) -> Result<Vec<pi_ai::Model>> {
    use globset::GlobBuilder;

    let mut available = pi_ai::get_providers()
        .into_iter()
        .flat_map(|provider| pi_ai::get_models(&provider))
        .collect::<Vec<_>>();
    available.sort_by(|left, right| {
        (&left.provider, &left.id).cmp(&(&right.provider, &right.id))
    });
    available.dedup_by(|left, right| {
        left.provider == right.provider && left.id == right.id
    });

    available = crate::models_config::filter_models_for_resolved_auth_async(available, None).await;
    let mut scoped = Vec::new();
    let mut seen = HashSet::new();
    for pattern in patterns {
        let trimmed = pattern.trim();
        let base_pattern = trimmed.rsplit_once(':').map_or(trimmed, |(base, suffix)| {
            matches!(suffix, "off" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max")
                .then_some(base)
                .unwrap_or(trimmed)
        });
        let matches = if base_pattern.contains(['*', '?', '[']) {
            let matcher = GlobBuilder::new(base_pattern)
                .case_insensitive(true)
                .build()
                .with_context(|| format!("invalid model pattern {pattern:?}"))?
                .compile_matcher();
            available
                .iter()
                .filter(|model| {
                    matcher.is_match(format!("{}/{}", model.provider, model.id))
                        || matcher.is_match(&model.id)
                })
                .cloned()
                .collect::<Vec<_>>()
        } else {
            available
                .iter()
                .filter(|model| {
                    format!("{}/{}", model.provider, model.id).eq_ignore_ascii_case(base_pattern)
                        || model.id.eq_ignore_ascii_case(base_pattern)
                })
                .cloned()
                .collect::<Vec<_>>()
        };
        if matches.is_empty() {
            warn_line(&format!("Warning: No models match pattern {pattern:?}"));
        }
        for model in matches {
            if seen.insert((model.provider.clone(), model.id.clone())) {
                scoped.push(model);
            }
        }
    }
    Ok(scoped)
}

pub(crate) fn resolve_prompt_input(
    input: &str,
    cwd: &Path,
    description: &str,
) -> Result<(String, Option<PathBuf>)> {
    let candidate = Path::new(input);
    let candidate = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        cwd.join(candidate)
    };
    if !candidate.exists() {
        return Ok((input.to_owned(), None));
    }
    let root = cwd
        .canonicalize()
        .with_context(|| format!("resolving working directory {}", cwd.display()))?;
    let canonical = candidate
        .canonicalize()
        .with_context(|| format!("resolving {description} file {}", candidate.display()))?;
    if !Path::new(input).is_absolute() && !canonical.starts_with(&root) {
        bail!("unsafe {description} path {input:?}: symlink escapes the working directory");
    }
    if !canonical.is_file() {
        bail!("{description} path is not a file: {}", canonical.display());
    }
    Ok((String::new(), Some(canonical)))
}

const SESSION_DIR_ENV: &str = "PI_CODING_AGENT_SESSION_DIR";

/// Resolve the single session storage and lookup directory for a working directory.
/// Precedence matches upstream: CLI, non-empty environment, effective settings,
/// then the existing per-cwd default.
pub(crate) fn effective_session_dir(
    cwd: &Path,
    cli_session_dir: Option<&Path>,
    settings_session_dir: Option<&Path>,
) -> Result<PathBuf> {
    let env_session_dir = std::env::var_os(SESSION_DIR_ENV).filter(|value| !value.is_empty());
    resolve_effective_session_dir(
        cwd,
        cli_session_dir,
        env_session_dir.as_deref().map(Path::new),
        settings_session_dir,
    )
}

fn resolve_effective_session_dir(
    cwd: &Path,
    cli_session_dir: Option<&Path>,
    env_session_dir: Option<&Path>,
    settings_session_dir: Option<&Path>,
) -> Result<PathBuf> {
    let configured = cli_session_dir
        .or(env_session_dir)
        .or(settings_session_dir);
    let Some(configured) = configured else {
        return pi_coding::canonical_path(&pi_coding::default_session_dir(cwd))
            .context("resolving default session directory");
    };
    let expanded = expand_session_dir_tilde(configured)?;
    pi_coding::canonical_path(&expanded).context("resolving effective session directory")
}

fn expand_session_dir_tilde(path: &Path) -> Result<PathBuf> {
    if path == Path::new("~") || path.strip_prefix(Path::new("~")).is_ok() {
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .ok_or_else(|| anyhow!("home directory is unavailable for session directory {}", path.display()))?;
        return Ok(if path == Path::new("~") {
            home
        } else {
            home.join(path.strip_prefix(Path::new("~")).expect("tilde prefix checked"))
        });
    }
    Ok(path.to_path_buf())
}

/// Environment variable honored for the active config profile when the
/// `--profile` flag is absent.
pub(crate) const PROFILE_ENV: &str = "PI_PROFILE";

/// Resolve the active config profile from the CLI and `PI_PROFILE`, validate
/// it, and install it process-wide so every agent-dir-derived path (settings,
/// auth, sessions, memory, skills, workflows) relocates under
/// `<base>/profiles/<name>`. `default`, empty, and whitespace select the
/// default profile (no relocation). Must run before any dispatch that
/// resolves an agent-dir-derived path.
pub(crate) fn activate_profile(cli: &Cli) -> Result<()> {
    let profile = resolve_active_profile(cli.profile.as_deref())?;
    pi_coding::set_active_profile(profile.as_deref());
    Ok(())
}

/// Resolve the active config profile name: the CLI `--profile` flag wins over
/// the `PI_PROFILE` environment variable; `default`, empty, or whitespace
/// selects the default profile (no relocation). Named profiles are validated
/// with an actionable error.
pub(crate) fn resolve_active_profile(cli_profile: Option<&str>) -> Result<Option<String>> {
    let env_profile = std::env::var(PROFILE_ENV)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|name| !name.is_empty());
    resolve_profile_precedence(
        cli_profile.map(str::trim).filter(|name| !name.is_empty()),
        env_profile.as_deref(),
    )
}

/// Pure precedence resolver for [`resolve_active_profile`], factored out so
/// CLI-over-env precedence and name validation are unit-testable without
/// mutating process environment.
fn resolve_profile_precedence(
    cli_profile: Option<&str>,
    env_profile: Option<&str>,
) -> Result<Option<String>> {
    match cli_profile.or(env_profile) {
        Some(name) if name == "default" => Ok(None),
        Some(name) => {
            crate::args::validate_profile_name(name).map_err(anyhow::Error::msg)?;
            Ok(Some(name.to_owned()))
        }
        None => Ok(None),
    }
}

/// Startup TTL for expired-session pruning: the `sessionTtlDays` setting when
/// present, otherwise [`pi_coding::DEFAULT_SESSION_TTL_DAYS`]. Settings
/// validation rejects `0`, and zero defensively falls back to the default
/// here rather than ever pruning everything.
fn session_ttl_from_settings(settings: &pi_coding::Settings) -> Duration {
    settings
        .session_ttl_days
        .filter(|days| *days > 0)
        .map(|days| Duration::from_secs(days.saturating_mul(24 * 60 * 60)))
        .unwrap_or(Duration::from_secs(
            pi_coding::DEFAULT_SESSION_TTL_DAYS * 24 * 60 * 60,
        ))
}

fn resolve_session_argument(
    argument: &str,
    cwd: &Path,
    session_dir: Option<&Path>,
) -> Result<PathBuf> {
    let path_like = argument.contains('/') || argument.contains('\\') || argument.ends_with(".jsonl");
    if path_like {
        let path = Path::new(argument);
        let path = if path.is_absolute() { path.to_path_buf() } else { cwd.join(path) };
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("reading explicit session path {argument:?}"))?;
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            bail!("explicit session path is not a regular file: {}", path.display());
        }
        if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl") {
            bail!("explicit session path is not a .jsonl file: {}", path.display());
        }
        return path
            .canonicalize()
            .with_context(|| format!("resolving explicit session path {argument:?}"));
    }
    let sessions = pi_coding::list_sessions_in(cwd, session_dir);
    if let Some(exact) = sessions.iter().find(|session| session.id == argument) {
        return Ok(exact.path.clone());
    }
    let matches = sessions
        .iter()
        .filter(|session| session.id.starts_with(argument))
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [] => bail!("No session found matching {argument:?}"),
        [session] => Ok(session.path.clone()),
        _ => bail!(
            "Session id prefix {argument:?} is ambiguous; matches: {}",
            matches.iter().map(|session| session.id.as_str()).collect::<Vec<_>>().join(", ")
        ),
    }
}



/// Stable per-process fallback identity for `--no-session` runs, where no
/// recorder exists to bind workflow storage to. Each process gets its own
/// namespace, so a fresh run never sees another run's workflows.
pub(crate) fn fallback_session_identity() -> &'static str {
    static TOKEN: OnceLock<String> = OnceLock::new();
    TOKEN.get_or_init(|| format!("proc-{}", uuid::Uuid::now_v7()))
}

/// Resolve per-session workflow storage roots: the workflow store and the
/// managed worktree root for the given session identity. The namespace is
/// session-scoped (repository digest + session id for git directories, session
/// id alone otherwise), so a resumed session (same session id) restores its
/// workflows while a new session id in the same repository starts empty.
pub(crate) fn workflow_storage_roots(cwd: &Path, agent_dir: &Path, session_id: &str) -> (PathBuf, PathBuf) {
    let namespace = pi_coding::workflow_worktree::WorkflowWorktreeManager::new(cwd)
        .session_namespace(session_id);
    (
        agent_dir.join("workflows").join(&namespace),
        agent_dir.join("workflow-worktrees").join(namespace),
    )
}

/// Rebind workflow storage to the ACTIVE session identity after a session
/// cutover (`/new`, `/fresh`, resume, fork, import). `setup_workflows` runs
/// once at startup with the initial session id; without this rebind every
/// later session in the process would keep showing (and mutating) the first
/// session's workflows. The roots are recomputed from the live recorder id +
/// cwd + agent dir, so a fresh session starts with an empty workflow list
/// while a resumed session (same id) restores its own workflows. No-op when
/// workflows were never configured (e.g. RPC-only applications).
pub(crate) async fn rebind_workflows_for_active_session(
    application: &pi_coding::Application,
) -> Result<()> {
    if application.workflow_manager().is_err() {
        return Ok(());
    }
    let Some((session_id, _)) = application.session().recorder_info() else {
        return Ok(());
    };
    let cwd = application.session().cwd().to_path_buf();
    let agent_dir = pi_coding::agent_dir_path();
    let (workflow_store_root, workflow_worktree_root) =
        workflow_storage_roots(&cwd, &agent_dir, &session_id);
    application
        .rebind_workflows(workflow_store_root, workflow_worktree_root)
        .await?;
    Ok(())
}

/// Load the process-wide model catalog: custom `models.json` entries, the
/// Radius catalog (best-effort), the llama.cpp router catalog, and a clean
/// runtime-key slate. Shared by [`build_session`] and the ACP agent mode.
pub(crate) async fn load_startup_catalogs(cli: &Cli) -> Result<()> {
    crate::models_config::load_custom_models()?;
    if let Err(error) = crate::models_config::load_radius_catalog(!offline()).await
        && cli.verbose
    {
        warn_line(&format!("Warning: Could not load Radius catalog: {error:#}"));
    }
    let llama = pi_coding::LlamaManager::default();
    if llama.effective_settings()?.is_some() && !offline() {
        if let Ok(refreshed) = llama.refresh_catalog().await
            && let Some(warning) = refreshed.warning
        {
            warn_line(&format!(
                "Warning: llama.cpp router unavailable; using cached catalog: {warning}"
            ));
        }
    } else if pi_ai::get_models(pi_ai::LLAMA_PROVIDER_ID).is_empty() {
        llama.load_cached_catalog()?;
    }
    for provider in pi_ai::get_providers() {
        crate::models_config::clear_runtime_api_key(&provider);
    }
    Ok(())
}

/// Resolve the canonical working directory and the resource-manager options
/// for a run, honoring CLI flags. `headless` is caller-supplied: interactive
/// TUI runs pass false, protocol modes (rpc/json/acp) pass true.
pub(crate) fn startup_resource_options(cli: &Cli, headless: bool) -> Result<(PathBuf, ResourceManagerOptions)> {
    let mut cwd: PathBuf = match &cli.cwd {
        Some(cwd) => cwd.clone(),
        None => std::env::current_dir().context("getting current directory")?,
    };
    cwd = cwd
        .canonicalize()
        .with_context(|| format!("resolving working directory {}", cwd.display()))?;
    if let Some(id) = cli.session_id.as_deref() {
        pi_coding::validate_session_id(id)?;
    }
    let mut resource_options = ResourceManagerOptions::new(&cwd);
    resource_options.headless = headless;
    resource_options.project_trust_override = if cli.approve {
        Some(true)
    } else if cli.no_approve {
        Some(false)
    } else {
        None
    };
    resource_options.explicit_extension_paths.clone_from(&cli.extensions);
    resource_options.explicit_skill_paths.clone_from(&cli.skills);
    resource_options.explicit_prompt_paths.clone_from(&cli.prompt_templates);
    resource_options.explicit_theme_paths.clone_from(&cli.themes);
    resource_options.disable_extensions = cli.no_extensions;
    resource_options.disable_skills = cli.no_skills;
    resource_options.disable_prompt_templates = cli.no_prompt_templates;
    resource_options.disable_themes = cli.no_themes;
    resource_options.disable_context_files = cli.no_context_files;
    Ok((cwd, resource_options))
}

/// Build the live [`Session`] from the parsed CLI flags, applying resume,
/// model restoration, and recording exactly as the Go CLI does.
pub async fn build_session(cli: &Cli) -> Result<RunSession> {
    load_startup_catalogs(cli).await?;
    let stdin_tty = std::io::IsTerminal::is_terminal(&std::io::stdin());
    let stdout_tty = std::io::IsTerminal::is_terminal(&std::io::stdout());
    let headless = matches!(cli.mode, Some(crate::args::Mode::Json | crate::args::Mode::Rpc))
        || cli.is_print_mode()
        || !stdin_tty
        || !stdout_tty;
    let (mut cwd, resource_options) = startup_resource_options(cli, headless)?;
    // Interactive TUI mode: capture settings diagnostics so they can be shown
    // in the UI after startup instead of vanishing into pre-TUI stderr. Non-
    // interactive modes keep the existing stderr behavior.
    let is_tui = !headless;
    if is_tui {
        pi_coding::arm_settings_diagnostic_capture();
    }
    let mut blueprint = RunSessionBlueprint::from_cli(
        cli,
        resource_options,
        matches!(extension_mode(cli), ExtensionMode::Tui | ExtensionMode::Rpc)
            .then(ExtensionUiAdapter::new),
    );
    let mut preview_resources = pi_coding::ResourceManager::new(
        blueprint.resource_options_for_startup(&cwd)?,
    )
    .context("loading settings and resources")?;
    let mut settings = preview_resources.snapshot().settings.clone();
    let session_dir = effective_session_dir(&cwd, cli.session_dir.as_deref(), settings.session_dir.as_deref())?;
    blueprint.set_session_dir(session_dir.clone());


    let resume_path: Option<PathBuf> = if let Some(input) = cli.resume.as_deref() {
        let sources = settings.effective_session_import_sources();
        Some(resolve_resume_for_startup(input, Some(&cwd), &session_dir, &sources)?.path)
    } else if let Some(argument) = cli.session.as_deref() {
        Some(resolve_session_argument(argument, &cwd, Some(&session_dir))?)
    } else if let Some(argument) = cli.fork.as_deref() {
        Some(resolve_session_argument(argument, &cwd, Some(&session_dir))?)
    } else if cli.continue_latest {
        // Deliberately native-only: `--continue` must never surprise-import a
        // foreign session merely because it is newer.
        pi_coding::list_sessions_in(&cwd, Some(&session_dir))
            .into_iter()
            .next()
            .map(|session| session.path)
    } else if let Some(id) = cli.session_id.as_deref() {
        pi_coding::list_sessions_in(&cwd, Some(&session_dir))
            .into_iter()
            .find(|session| session.id == id)
            .map(|session| session.path)
    } else {
        None
    };

    let mut resume_ctx: Option<BranchContext> = None;
    let mut resume_has_thinking_entry = false;
    if let Some(path) = &resume_path {
        let tree = pi_coding::load_session_tree(path)
            .with_context(|| format!("loading session {}", path.display()))?;
        if cli.cwd.is_none() && !tree.header.cwd.as_os_str().is_empty() && tree.header.cwd.exists() {
            // Restore the session's recorded cwd only when it still exists;
            // an imported/foreign session may record a cwd that is absent on
            // this machine — fall back to the current cwd rather than fail.
            cwd = tree.header.cwd.canonicalize().with_context(|| {
                format!("resolving resumed working directory {}", tree.header.cwd.display())
            })?;
        }
        resume_has_thinking_entry = tree.has_thinking_entry();
        resume_ctx = Some(tree.build_context(None));
        preview_resources = preview_resources
            .rebuild_for_cwd(&cwd)
            .context("reloading settings and resources for resumed working directory")?;
        settings = preview_resources.snapshot().settings.clone();
    }

    if cli.verbose {
        for diagnostic in preview_resources.diagnostics() {
            let path = diagnostic.path.as_ref().map_or(String::new(), |path| {
                format!(" ({})", path.display())
            });
            eprintln!("{:?}: {}{}", diagnostic.level, diagnostic.message, path);
        }
    }
    // Collect non-fatal startup warnings for TUI display. Settings diagnostics
    // were captured (stderr suppressed) when the TUI capture is armed; resource
    // diagnostics are collected from the snapshot. Error-level resource
    // diagnostics already bailed inside build_candidate, so only warnings remain.
    let mut startup_warnings = if is_tui {
        pi_coding::drain_settings_diagnostics()
    } else {
        Vec::new()
    };
    if is_tui {
        for diagnostic in preview_resources.diagnostics() {
            let path = diagnostic.path.as_ref().map_or(String::new(), |path| {
                format!(" ({})", path.display())
            });
            startup_warnings.push(format!("{:?}: {}{}", diagnostic.level, diagnostic.message, path));
        }
    }
    let model_patterns = cli.models.as_deref().or_else(|| settings.scoped_model_patterns());
    let scoped_models = match model_patterns {
        Some(patterns) => Some(resolve_model_scope(patterns).await?),
        None => None,
    };

    // 3. Resolve the initial model: explicit CLI, authenticated resumed model,
    //    authenticated settings default, then the first authenticated model.
    let (model, api_key, parsed_think) =
        resolve_initial_model(cli, &settings, resume_ctx.as_ref()).await?;

    // 4. Thinking priority: --think, model suffix, resumed recorded thinking,
    //    settings default, then the existing medium default.
    let thinking_level = resolve_initial_thinking_level(
        cli.think.as_deref(),
        &parsed_think,
        resume_ctx.as_ref(),
        resume_has_thinking_entry,
        settings.default_thinking_level.map(thinking_level_str),
    );

    let options = SessionOptions {
        model: model.clone(),
        cwd: cwd.clone(),
        system_prompt: String::new(),
        thinking_level,
        api_key: api_key.clone(),
        compaction: Some(DEFAULT_COMPACTION_SETTINGS),
        stream_options: Default::default(),
        tools: None,
        before_tool_call: None,
        after_tool_call: None,
        stream_fn: None,
        auth_resolver: None,
    };
    let candidate = blueprint.build(&cwd, options).await?;
    let crate::session_run_blueprint::RunSessionCandidate {
        session,
        extension_runtime: runtime,
        extension_permissions: permissions,
        orchestration,
        goal_tool,
    } = candidate;
    session.set_session_dir(session_dir.clone());

    let setup_result: Result<()> = async {
        if let Some(path) = &resume_path {
            let ctx = resume_ctx.expect("resume context");
            let messages_len = ctx.messages.len();
            session.load_history(ctx.messages).await?;
            eprintln!(
                "\x1b[2mresumed {} messages from {}\x1b[0m",
                messages_len,
                path.display()
            );
            if !cli.no_session {
                let recorder = if cli.fork.is_some() {
                    pi_coding::fork_session_in(
                        path,
                        &cwd,
                        Some(&session_dir),
                        cli.session_id.as_deref(),
                    )?
                } else {
                    pi_coding::resume_session(path)?
                };
                if !resume_has_thinking_entry {
                    recorder.record_thinking_level(thinking_level_str(thinking_level))?;
                }
                session.record(recorder)?;
            }
        } else if !cli.no_session {
            let recorder = pi_coding::start_session_in(
                &cwd,
                Some(&model),
                Some(thinking_level_str(thinking_level)),
                Some(&session_dir),
                cli.session_id.as_deref(),
                None,
            )?;
            session.record(recorder)?;
        }
        if let Some(name) = cli.name.as_deref() {
            session.set_session_name(name)?;
        }
        Ok(())
    }
    .await;
    if let Err(error) = setup_result {
        session.abort().await;
        session.wait_for_idle().await;
        if let Some(orchestration) = orchestration {
            orchestration.shutdown().await;
        }
        runtime.shutdown().await;
        return Err(error);
    }

    // Best-effort TTL cleanup of expired native session files, run after
    // resume/start resolution so the current run's session file is known and
    // skipped (a resumed session may itself be older than the TTL). The prune
    // swallows I/O errors, so cleanup can never fail startup.
    {
        let native_root = pi_coding::native_sessions_root();
        let mut prune_roots = Vec::new();
        if !session_dir.starts_with(&native_root) {
            prune_roots.push(session_dir.clone());
        }
        prune_roots.push(native_root);
        let mut prune_skip = Vec::new();
        if let Some(path) = &resume_path {
            prune_skip.push(path.clone());
        }
        if let Some((_, path)) = session.recorder_info() {
            prune_skip.push(path);
        }
        // Directory-level guard: the prune best-effort-removes emptied
        // per-cwd dirs, and a just-started auto-id recorder has NO file on
        // disk yet, so its dir is empty and would be deleted before the first
        // flush (ENOENT on the next persist). Never remove this run's session
        // dir root, nor the parent of the current/resumed session file.
        let mut prune_dir_skip = Vec::new();
        prune_dir_skip.push(session_dir.clone());
        if let Some(path) = &resume_path {
            if let Some(parent) = path.parent() {
                prune_dir_skip.push(parent.to_path_buf());
            }
        }
        if let Some((_, path)) = session.recorder_info() {
            if let Some(parent) = path.parent() {
                prune_dir_skip.push(parent.to_path_buf());
            }
        }
        let _ = pi_coding::prune_expired_sessions(
            &prune_roots,
            std::time::SystemTime::now(),
            session_ttl_from_settings(&settings),
            &prune_skip,
            &prune_dir_skip,
        );
    }

    let application = Application::new_with_extensions(session, runtime, permissions).await;
    application.attach_runtime_factory(Arc::new(blueprint.clone()))?;
    if resume_path.is_some() {
        application.prepare_resumed_goal(cli.fork.is_some())?;
    }
    if let Some(binding) = goal_tool {
        application.attach_goal_tool(binding)?;
    }
    if let Some(orchestration) = orchestration {
        if let Err(error) = application.attach_orchestration(orchestration) {
            application.cleanup().await;
            return Err(error);
        }
    }
    let agent_dir = pi_coding::agent_dir_path();
    // Workflow storage is scoped to the owning session: the recorder's id for
    // recorded sessions (new or resumed), a stable per-process token for
    // `--no-session` runs where no recorder exists.
    let session_identity = application
        .session()
        .recorder_info()
        .map(|(id, _)| id)
        .unwrap_or_else(|| fallback_session_identity().to_owned());
    let (workflow_store_root, workflow_worktree_root) =
        workflow_storage_roots(&cwd, &agent_dir, &session_identity);
    if let Err(error) = application
        .setup_workflows(cwd.clone(), workflow_store_root, workflow_worktree_root)
        .await
    {
        application.cleanup().await;
        return Err(error);
    }
    // Drain any settings diagnostics emitted after the initial resource load
    // (e.g. by blueprint.build which reloads resources). Due to process-wide
    // dedupe, these are typically empty unless new paths were loaded.
    if is_tui {
        startup_warnings.extend(pi_coding::drain_settings_diagnostics());
    }
    Ok(RunSession {
        application,
        extension_ui: blueprint.extension_ui(),
        model,
        scoped_models,
        startup_warnings,
        spawner: RunSessionSpawner::from_startup(cli, session_dir),
    })
}

/// Run a single prompt in print mode through the shared [`Application`]
/// lifecycle and human event renderer. Returns the final assistant text.
pub async fn run_print_to<W>(
    application: &Application,
    prompt: &str,
    writer: &mut W,
    ansi: bool,
) -> Result<String>
where
    W: std::io::Write,
{
    if let Some(argument) = prompt.trim().strip_prefix("/goal") {
        let command = crate::goal_commands::parse_interactive_goal_command(
            (!argument.trim().is_empty()).then_some(argument.trim()),
        )?;
        let output = crate::goal_commands::execute_interactive_goal_command(application, command).await?;
        writer.write_all(output.as_bytes())?;
        writer.write_all(b"\n")?;
        writer.flush()?;
        return Ok(output);
    }
    if let Some(argument) = prompt.trim().strip_prefix("/workflow") {
        // Exact prefix: "/workflow" must not match "/workfloww".
        if argument.is_empty() || argument.starts_with(char::is_whitespace) {
            let rest = argument.trim_start();
            let command = crate::workflow_commands::parse_interactive_workflow_command(
                (!rest.is_empty()).then_some(rest),
            )?;
            let effect =
                crate::workflow_commands::execute_interactive_workflow_on_application(
                    application,
                    command,
                )
                .await?;
            let output = match effect {
                crate::workflow_commands::WorkflowCommandEffect::OpenPage => {
                    "Open the workflows page in the full-screen TUI (bare /workflow).".to_owned()
                }
                crate::workflow_commands::WorkflowCommandEffect::Message(message) => message,
            };
            writer.write_all(output.as_bytes())?;
            writer.write_all(b"\n")?;
            writer.flush()?;
            return Ok(output);
        }
    }
    if prompt.trim().is_empty() {
        bail!("print mode requires a prompt");
    }
    crate::human_event_renderer::run_human_turn_to(application, prompt, writer, ansi).await
}

/// Run a single prompt in print mode, streaming to stdout.
pub async fn run_print(application: &Application, prompt: &str) -> Result<String> {
    use std::io::IsTerminal as _;

    let mut stdout = std::io::stdout();
    let ansi = stdout.is_terminal();
    run_print_to(application, prompt, &mut stdout, ansi).await
}

/// `rpi --print` / non-interactive text entrypoint.
pub async fn print_mode(cli: &Cli) -> Result<()> {
    let prompts = cli
        .prompt
        .iter()
        .filter(|prompt| !prompt.is_empty())
        .collect::<Vec<_>>();
    if prompts.is_empty() {
        bail!("print mode requires a prompt");
    }
    let RunSession { application, .. } = build_session(cli).await?;
    let result = async {
        for prompt in prompts {
            run_print(&application, prompt).await?;
        }
        Ok(())
    }
    .await;
    application.cleanup().await;
    result
}

pub(crate) fn extension_startup_error(report: &pi_coding::ExtensionLoadReport) -> anyhow::Error {
    anyhow!(
        "extension startup rejected {} extension(s): {}",
        report.failures.len(),
        report
            .failures
            .iter()
            .map(|failure| format!(
                "{} ({}): {}",
                failure.extension_id,
                failure.path.display(),
                failure.message
            ))
            .collect::<Vec<_>>()
            .join("; ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use parking_lot::Mutex;
    use pi_agent::ThinkingLevel;
    use pi_ai::{Model, SimpleStreamOptions};
    use pi_coding::{
        AgentDefinition, AgentDefinitionSource, ChildSessionFactory, GoalToolBinding,
        OrchestrationRuntime, ResourceDiscovery, ResourceSnapshot, Session, Settings, TaskItem,
        ToolSelection, TrustDecision, TrustResolution,
    };

    fn blueprint(cli: &Cli) -> RunSessionBlueprint {
        RunSessionBlueprint::from_cli(
            cli,
            ResourceManagerOptions::new(std::env::current_dir().expect("cwd")),
            None,
        )
    }


    #[test]
    fn effective_session_dir_obeys_cli_env_settings_default_precedence() {
        let cwd = tempfile::tempdir().expect("cwd");
        let cli = cwd.path().join("cli");
        let env = cwd.path().join("env");
        let settings = cwd.path().join("settings");

        assert_eq!(
            resolve_effective_session_dir(cwd.path(), Some(&cli), Some(&env), Some(&settings))
                .expect("CLI"),
            cli
        );
        assert_eq!(
            resolve_effective_session_dir(cwd.path(), None, Some(&env), Some(&settings))
                .expect("environment"),
            env
        );
        assert_eq!(
            resolve_effective_session_dir(cwd.path(), None, None, Some(&settings))
                .expect("settings"),
            settings
        );
        assert_eq!(
            resolve_effective_session_dir(cwd.path(), None, None, None).expect("default"),
            pi_coding::default_session_dir(cwd.path())
        );
        assert_eq!(
            resolve_effective_session_dir(
                cwd.path(),
                None,
                None,
                Some(Path::new("relative-sessions")),
            )
            .expect("relative settings"),
            pi_coding::canonical_path(Path::new("relative-sessions")).expect("relative path")
        );
    }

    #[test]
    fn effective_session_dir_ignores_empty_environment_value() {
        let cwd = tempfile::tempdir().expect("cwd");
        let settings = cwd.path().join("settings");
        let env_session_dir = std::ffi::OsStr::new("");
        let env_session_dir = (!env_session_dir.is_empty()).then(|| Path::new(env_session_dir));
        assert_eq!(
            resolve_effective_session_dir(cwd.path(), None, env_session_dir, Some(&settings))
                .expect("settings"),
            settings
        );
    }

    #[test]
    fn profile_precedence_cli_wins_over_env() {
        // CLI --profile beats PI_PROFILE.
        assert_eq!(
            resolve_profile_precedence(Some("cli-work"), Some("env-work"))
                .expect("CLI wins"),
            Some("cli-work".to_owned())
        );
        // Env is used when no flag is present.
        assert_eq!(
            resolve_profile_precedence(None, Some("env-work")).expect("env honored"),
            Some("env-work".to_owned())
        );
        // Neither source selects the default profile.
        assert_eq!(
            resolve_profile_precedence(None, None).expect("no profile"),
            None
        );
        // The flag position does not matter; empty values are filtered before
        // this resolver runs.
        assert_eq!(
            resolve_profile_precedence(Some("work"), Some("")).expect("empty env ignored"),
            Some("work".to_owned())
        );
    }

    #[test]
    fn profile_precedence_default_selects_default_profile() {
        assert_eq!(
            resolve_profile_precedence(Some("default"), None).expect("explicit default"),
            None
        );
        assert_eq!(
            resolve_profile_precedence(None, Some("default")).expect("env default"),
            None
        );
        // The CLI flag wins over the environment in either direction.
        assert_eq!(
            resolve_profile_precedence(Some("work"), Some("default"))
                .expect("named CLI beats env default"),
            Some("work".to_owned())
        );
        assert_eq!(
            resolve_profile_precedence(Some("default"), Some("env-work"))
                .expect("explicit CLI default beats env profile"),
            None
        );
    }

    #[test]
    fn profile_precedence_validates_names_actionably() {
        for (cli, env) in [
            (Some("bad/name"), None),
            (None, Some("bad/name")),
            (Some("with space"), None),
        ] {
            let error = resolve_profile_precedence(cli, env)
                .expect_err("invalid profile name must fail");
            let message = format!("{error:#}");
            assert!(
                message.contains("profile name") && message.contains("letters, digits"),
                "error must be actionable, got: {message}"
            );
        }
        // A valid CLI profile shadows an invalid environment value entirely.
        assert_eq!(
            resolve_profile_precedence(Some("x"), Some("bad/name")).expect("CLI wins"),
            Some("x".to_owned())
        );
    }

    #[test]
    fn profile_precedence_accepts_valid_and_max_length_names() {
        let max = "a".repeat(crate::args::MAX_PROFILE_NAME_LENGTH);
        assert_eq!(
            resolve_profile_precedence(Some(&max), None).expect("max length valid"),
            Some(max)
        );
        assert_eq!(
            resolve_profile_precedence(Some("my-profile_2"), None).expect("charset valid"),
            Some("my-profile_2".to_owned())
        );
    }

    #[test]
    fn session_ttl_defaults_to_30_days_and_honors_the_setting() {
        assert_eq!(
            session_ttl_from_settings(&Settings::default()),
            Duration::from_secs(30 * 24 * 60 * 60)
        );
        let mut settings = Settings::default();
        settings.session_ttl_days = Some(7);
        assert_eq!(
            session_ttl_from_settings(&settings),
            Duration::from_secs(7 * 24 * 60 * 60)
        );
        // Zero (rejected by settings validation) defensively falls back to the
        // default rather than pruning everything.
        settings.session_ttl_days = Some(0);
        assert_eq!(
            session_ttl_from_settings(&settings),
            Duration::from_secs(30 * 24 * 60 * 60)
        );
    }

    #[test]
    fn workflow_storage_roots_are_stable_and_session_scoped() {
        fn init_repo(path: &Path) {
            fs::create_dir_all(path).expect("repo directory");
            for args in [
                vec!["init"],
                vec!["config", "user.name", "Pi Test"],
                vec!["config", "user.email", "pi@example.test"],
            ] {
                let status = std::process::Command::new("git")
                    .args(["-c", "init.defaultBranch=main"])
                    .args(args)
                    .current_dir(path)
                    .status()
                    .expect("git command");
                assert!(status.success());
            }
            fs::write(path.join("README.md"), "base\n").expect("base file");
            let status = std::process::Command::new("git")
                .args(["add", "README.md"])
                .current_dir(path)
                .status()
                .expect("git add");
            assert!(status.success());
            let status = std::process::Command::new("git")
                .args(["-c", "commit.gpgsign=false", "commit", "-m", "initial"])
                .current_dir(path)
                .status()
                .expect("git commit");
            assert!(status.success());
        }

        let sandbox = tempfile::tempdir().expect("sandbox");
        let agent_dir = sandbox.path().join("agent");
        let first = sandbox.path().join("first");
        let second = sandbox.path().join("second");
        init_repo(&first);
        init_repo(&second);

        // Same session id resolves to the same roots, deterministically.
        let first_roots = workflow_storage_roots(&first, &agent_dir, "session-a");
        assert_eq!(first_roots, workflow_storage_roots(&first, &agent_dir, "session-a"));
        // Different session ids in the same repository are isolated.
        assert_ne!(first_roots, workflow_storage_roots(&first, &agent_dir, "session-b"));
        // Different repositories keep distinct namespaces.
        assert_ne!(first_roots, workflow_storage_roots(&second, &agent_dir, "session-a"));
        assert!(first_roots.0.starts_with(agent_dir.join("workflows")));
        assert!(first_roots.1.starts_with(agent_dir.join("workflow-worktrees")));

        // Non-git directories still resolve a valid, session-scoped namespace.
        let plain = sandbox.path().join("plain");
        fs::create_dir_all(&plain).expect("plain directory");
        let plain_roots = workflow_storage_roots(&plain, &agent_dir, "session-a");
        assert_eq!(plain_roots, workflow_storage_roots(&plain, &agent_dir, "session-a"));
        assert_ne!(plain_roots, workflow_storage_roots(&plain, &agent_dir, "session-b"));
    }

    #[test]
    fn settings_and_cli_selection_drive_identical_initial_tool_gates() {
        let mut settings = Settings::default();
        settings.orchestration = Some(pi_coding::OrchestrationSettings {
            tasks: Some(true),
            process: Some(true),
            todo: Some(true),
            glob: Some(true),
            ..pi_coding::OrchestrationSettings::default()
        });
        let cli = <Cli as clap::Parser>::try_parse_from(["rpi"]).expect("default cli");
        let gates = blueprint(&cli).test_tool_gates(&settings);
        assert_eq!(gates, (true, true, true, true, true));

        let selected = <Cli as clap::Parser>::try_parse_from([
            "rpi",
            "--tools",
            "task,hub,process,todo,glob,goal",
        ])
        .expect("selected tools");
        let gates = blueprint(&selected).test_tool_gates(&Settings::default());
        assert_eq!(gates, (true, true, true, true, true));

        let excluded = <Cli as clap::Parser>::try_parse_from([
            "rpi",
            "--exclude-tools",
            "goal",
        ])
        .expect("excluded goal tool");
        let gates = blueprint(&excluded).test_tool_gates(&Settings::default());
        assert!(!gates.4);

        let disabled = <Cli as clap::Parser>::try_parse_from(["rpi", "--no-tools"])
            .expect("disabled tools");
        let gates = blueprint(&disabled).test_tool_gates(&settings);
        assert_eq!(gates, (false, false, false, false, false));
    }

    #[test]
    fn goal_catalog_defaults_on_but_no_tools_and_explicit_exclude_win() {
        let cwd = tempfile::tempdir().expect("cwd");
        let options = || SessionOptions {
            model: Model::default(),
            cwd: cwd.path().to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: String::new(),
            compaction: None,
            stream_options: SimpleStreamOptions::default(),
            tools: Some(Vec::new()),
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: None,
            auth_resolver: None,
        };
        let with_selection = |selection: ToolSelection| {
            Session::new_with_additional_tools_filtered_and_discovery(
                options(),
                vec![GoalToolBinding::default().tool()],
                selection,
                ResourceDiscovery::Disabled,
            )
            .expect("session")
            .get_active_tool_names()
        };

        assert_eq!(with_selection(ToolSelection::default()), ["goal"]);
        assert!(with_selection(ToolSelection {
            deny: vec!["goal".to_owned()],
            ..ToolSelection::default()
        })
        .is_empty());
        assert!(with_selection(ToolSelection {
            disable_all: true,
            ..ToolSelection::default()
        })
        .is_empty());
    }

    #[test]
    fn settings_thinking_level_is_used_after_cli_model_and_resume_priorities() {
        assert_eq!(
            resolve_initial_thinking_level(None, "", None, false, Some("high")),
            pi_agent::ThinkingLevel::High
        );
        assert_eq!(
            resolve_initial_thinking_level(Some("low"), "", None, false, Some("high")),
            pi_agent::ThinkingLevel::Low
        );
    }

    #[tokio::test]
    async fn initial_cli_orchestration_inherits_selected_parent_model() {
        let artifacts = tempfile::tempdir().expect("artifacts root");
        let parent_model = Model {
            id: "selected-parent".to_owned(),
            name: "Selected Parent".to_owned(),
            api: "test-api".to_owned(),
            provider: "test-provider".to_owned(),
            ..Model::default()
        };
        let bundled = AgentDefinition { name: "task".to_owned(),
        description: "bundled task agent".to_owned(),
        system_prompt: "do the task".to_owned(),
        tools: Some(Vec::new()),
        autoload_skills: Vec::new(),
        // No definition model list — must fall through to parent.
        model: None,
        thinking_level: Some(ThinkingLevel::Off),
        max_turns: None,
        max_tool_calls: None,
        timeout_secs: None,
        disallowed_tools: Vec::new(),
        capability_ceiling: None,
        source: AgentDefinitionSource::Bundled,
        path: None, trusted: true, kind: pi_coding::AgentDefinitionKind::Agent, personality: None, soft_budget: None };
        let snapshot = ResourceSnapshot {
            generation: 1,
            cwd: artifacts.path().to_path_buf(),
            trust: TrustResolution {
                decision: TrustDecision::Trusted,
                matched_path: None,
                project_path: artifacts.path().to_path_buf(),
            },
            settings: Settings::default(),
            context_files: Vec::new(),
            skills: Vec::new(),
            agents: vec![bundled],
            prompts: Vec::new(),
            themes: Vec::new(),
            package_extensions: Vec::new(),
            theme_dirs: Vec::new(),
            keybinding_files: Vec::new(),
            system_prompt: None,
            append_system_prompt: Vec::new(),
            diagnostics: Vec::new(),
        };
        // Empty settings.agents → no per-agent model override.
        let settings = Settings::default();
        let config = crate::session_run_blueprint::test_orchestration_config(
            &snapshot,
            &settings,
            &parent_model,
        );
        assert_eq!(config.parent_model.provider, parent_model.provider);
        assert_eq!(config.parent_model.id, parent_model.id);

        let seen = Arc::new(Mutex::new(None::<Model>));
        let seen_factory = seen.clone();
        let factory: ChildSessionFactory = Arc::new(move |request| {
            let seen_factory = seen_factory.clone();
            Box::pin(async move {
                *seen_factory.lock() = Some(request.model.clone());
                // Debug must never surface secret material from the request.
                let debug = format!("{request:?}");
                assert!(
                    !debug.contains("sk-") && !debug.to_ascii_lowercase().contains("api_key"),
                    "ChildSessionRequest debug leaked secrets: {debug}"
                );
                Session::new(SessionOptions {
                    model: request.model,
                    cwd: std::env::current_dir().expect("cwd"),
                    system_prompt: request.system_prompt,
                    thinking_level: request.thinking_level.unwrap_or(ThinkingLevel::Off),
                    api_key: String::new(),
                    compaction: None,
                    stream_options: SimpleStreamOptions::default(),
                    tools: Some(Vec::new()),
                    before_tool_call: None,
                    after_tool_call: None,
                    stream_fn: None,
                    auth_resolver: None,
                })
            })
        });

        let runtime = OrchestrationRuntime::new(config, factory).expect("runtime");
        let _ = runtime
            .spawn_tasks(
                "Main",
                0,
                vec![TaskItem {
                    index: 0,
                    id: "Child".to_owned(),
                    agent: "task".to_owned(),
                    assignment: "inherit parent model".to_owned(),
                    todo_task_id: None,
                    ..Default::default()
                }],
            )
            .expect("spawn");
        for _ in 0..50 {
            if seen.lock().is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let captured = seen
            .lock()
            .clone()
            .expect("factory observed ChildSessionRequest.model");
        assert_eq!(captured.provider, parent_model.provider);
        assert_eq!(captured.id, parent_model.id);
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn bind_parent_session_binds_and_recovers_existing_sidecar() {
        let artifacts = tempfile::tempdir().expect("artifacts");
        let session_dir = tempfile::tempdir().expect("session dir");
        let test_model = pi_ai::Model {
            id: "durable-bind-test".to_owned(),
            name: "Durable Bind Test".to_owned(),
            api: "durable-bind-test".to_owned(),
            provider: "durable-bind-test".to_owned(),
            ..pi_ai::Model::default()
        };
        // Create a parent session with a recorder.
        let recorder = pi_coding::start_session_in(
            artifacts.path(),
            Some(&test_model),
            None,
            Some(session_dir.path()),
            None,
            None,
        )
        .expect("parent recorder");
        let parent = Session::new(pi_coding::SessionOptions {
            model: test_model.clone(),
            cwd: artifacts.path().to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: String::new(),
            compaction: None,
            stream_options: Default::default(),
            tools: Some(Vec::new()),
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: None,
            auth_resolver: None,
        })
        .expect("parent session");
        parent.record(recorder).expect("record");

        let factory: ChildSessionFactory = Arc::new(|_| {
            Box::pin(async { Err(anyhow!("test factory should not be called")) })
        });
        let mut config = pi_coding::OrchestrationConfig::new(
            pi_coding::AgentCatalog::from_agents(vec![pi_coding::AgentDefinition {
                name: "task".to_owned(),
                description: "task".to_owned(),
                system_prompt: "task".to_owned(),
                tools: Some(Vec::new()),
                autoload_skills: Vec::new(),
                model: None,
                thinking_level: Some(ThinkingLevel::Off),
                max_turns: None,
                max_tool_calls: None,
                timeout_secs: None,
                disallowed_tools: Vec::new(),
                capability_ceiling: None,
                source: pi_coding::AgentDefinitionSource::Bundled,
                path: None,
                trusted: true,
                kind: pi_coding::AgentDefinitionKind::Agent,
                personality: None,
                soft_budget: None,
            }]),
            artifacts.path(),
        );
        config.parent_model = test_model;
        config.idle_ttl = None;
        let runtime = OrchestrationRuntime::new(config, factory).expect("runtime");

        runtime.bind_parent_session(&parent).expect("bind");
        assert!(runtime.is_durable());
        assert!(
            runtime.recover().is_err(),
            "plain bind must not create or overwrite missing durable state"
        );
        runtime
            .recover_or_initialize()
            .expect("initialize fresh durable state");
        runtime.recover().expect("recover initialized state");
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn bind_parent_session_fails_without_recorder() {
        let artifacts = tempfile::tempdir().expect("artifacts");
        let parent = Session::new(pi_coding::SessionOptions {
            model: pi_ai::Model::default(),
            cwd: artifacts.path().to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: String::new(),
            compaction: None,
            stream_options: Default::default(),
            tools: Some(Vec::new()),
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: None,
            auth_resolver: None,
        })
        .expect("parent session");
        // No recorder attached — bind should fail.
        let factory: ChildSessionFactory = Arc::new(|_| {
            Box::pin(async { Err(anyhow!("should not be called")) })
        });
        let config = pi_coding::OrchestrationConfig::new(
            pi_coding::AgentCatalog::from_agents(vec![pi_coding::AgentDefinition {
                name: "task".to_owned(),
                description: "task".to_owned(),
                system_prompt: "task".to_owned(),
                tools: Some(Vec::new()),
                autoload_skills: Vec::new(),
                model: None,
                thinking_level: Some(ThinkingLevel::Off),
                max_turns: None,
                max_tool_calls: None,
                timeout_secs: None,
                disallowed_tools: Vec::new(),
                capability_ceiling: None,
                source: pi_coding::AgentDefinitionSource::Bundled,
                path: None,
                trusted: true,
                kind: pi_coding::AgentDefinitionKind::Agent,
                personality: None,
                soft_budget: None,
            }]),
            artifacts.path(),
        );
        let runtime = OrchestrationRuntime::new(config, factory).expect("runtime");
        let error = runtime
            .bind_parent_session(&parent)
            .expect_err("bind without recorder should fail");
        assert!(error.to_string().contains("recording is unavailable"));
        assert!(!runtime.is_durable());
        runtime.shutdown().await;
    }
}
