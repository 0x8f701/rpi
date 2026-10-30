//! Project context discovery (AGENTS.md/CLAUDE.md ancestry + global `.rpi`)
//! and Agent Skill discovery/frontmatter. Port of `coding/resources.go`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use pi_sandbox::AbsolutePath;
use serde::{Deserialize, Serialize};

use anyhow::{Context, Result, bail};

use crate::fs::{
    CapabilityAccess, CapabilityEntry, CapabilityFs, CapabilityRootLease, CapabilityRootPurpose,
    FsError, RootCreation,
};
use crate::system_prompt::ContextFile;
/// Converts a path to a String losslessly (paths are UTF-8 in practice; mirrors
/// Go's filepath string results). `Path`/`PathBuf` have no `Display`.
fn ps(p: impl AsRef<Path>) -> String {
    p.as_ref().to_string_lossy().into_owned()
}

pub const CONFIG_DIR_NAME: &str = ".rpi";

pub const MAX_RESOURCE_FILE_BYTES: u64 = 1024 * 1024;

pub const MAX_RESOURCE_SNAPSHOT_BYTES: usize = 8 * 1024 * 1024;

/// Per-candidate bounds for the actionable startup error. The message is
/// path + typed reason only (never file contents), so a hostile or enormous
/// diagnostic cannot flood the journal.
pub const MAX_CONTEXT_DIAGNOSTICS: usize = 8;
pub const MAX_CONTEXT_DISPLAY_PATH: usize = 240;
pub const MAX_CONTEXT_DISPLAY_REASON: usize = 400;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextFileDiagnostic {
    pub path: PathBuf,
    pub message: String,
}

#[derive(Clone, Debug)]
pub struct ContextFileLoadError {
    pub diagnostics: Vec<ContextFileDiagnostic>,
}

impl std::fmt::Display for ContextFileLoadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "context file validation failed for {} candidate(s)",
            self.diagnostics.len()
        )?;
        for diagnostic in self.diagnostics.iter().take(MAX_CONTEXT_DIAGNOSTICS) {
            let path =
                crate::redact::redact_bounded(&ps(&diagnostic.path), MAX_CONTEXT_DISPLAY_PATH);
            let reason =
                crate::redact::redact_bounded(&diagnostic.message, MAX_CONTEXT_DISPLAY_REASON);
            write!(
                formatter,
                "\n- {}: {}",
                if path.is_empty() { "<unknown>" } else { &path },
                if reason.is_empty() {
                    "unavailable reason"
                } else {
                    &reason
                }
            )?;
        }
        if self.diagnostics.len() > MAX_CONTEXT_DIAGNOSTICS {
            write!(
                formatter,
                "\n- ... and {} more candidate(s) omitted",
                self.diagnostics.len() - MAX_CONTEXT_DIAGNOSTICS
            )?;
        }
        Ok(())
    }
}

impl std::error::Error for ContextFileLoadError {}

pub fn context_file_diagnostics(error: &anyhow::Error) -> Option<Vec<ContextFileDiagnostic>> {
    error
        .downcast_ref::<ContextFileLoadError>()
        .map(|error| error.diagnostics.clone())
}

/// Read one UTF-8 resource without allocating beyond the per-file cap.
pub fn read_resource_text(path: &Path, kind: &str) -> anyhow::Result<String> {
    let capabilities = CapabilityFs::new()?;
    read_resource_text_with_capabilities(&capabilities, path, kind)
}

pub(crate) fn read_resource_text_with_capabilities(
    capabilities: &Arc<CapabilityFs>,
    path: &Path,
    kind: &str,
) -> anyhow::Result<String> {
    let absolute = absolute_resource_path(path)?;
    let file = capabilities
        .open_read_file_allowing_link(CapabilityRootPurpose::ProjectResources, absolute)
        .with_context(|| format!("opening {kind} {}", path.display()))?;
    let bytes = file
        .read_bounded(MAX_RESOURCE_FILE_BYTES)
        .with_context(|| format!("reading {kind} {}", path.display()))?;
    String::from_utf8(bytes).with_context(|| format!("decoding {kind} {} as UTF-8", path.display()))
}

pub(crate) fn read_resource_text_from_root(
    root: &CapabilityRootLease,
    relative: &Path,
    kind: &str,
) -> anyhow::Result<String> {
    let file = root
        .open_file_allowing_link(relative)
        .with_context(|| format!("opening {kind} {}", root.display_root().join(relative).display()))?;
    let bytes = file
        .read_bounded(MAX_RESOURCE_FILE_BYTES)
        .with_context(|| format!("reading {kind} {}", root.display_root().join(relative).display()))?;
    String::from_utf8(bytes).with_context(|| {
        format!("decoding {kind} {} as UTF-8", root.display_root().join(relative).display())
    })
}

/// Strict (NO_SYMLINKS) read of a PROJECT context candidate (AGENTS.md /
/// CLAUDE.md under the project). A project context file must never follow a
/// symbolic link, even when the project is trusted: only platform
/// global/agent-managed roots resolve managed-config links. A link on the
/// project path is rejected with an explicit "symbolic link" diagnostic
/// rather than reading the target.
fn read_project_context_file_text(
    capabilities: &Arc<CapabilityFs>,
    path: &Path,
) -> anyhow::Result<String> {
    let absolute = absolute_resource_path(path)?;
    let file = match capabilities.open_exact_file(CapabilityRootPurpose::ProjectResources, absolute)
    {
        Ok(file) => file,
        Err(error) if crate::fs::is_link_blocked(&error) => {
            bail!("context file is a symbolic link")
        }
        Err(error) => {
            return Err(error).with_context(|| format!("opening context file {}", path.display()));
        }
    };
    let bytes = file
        .read_bounded(MAX_RESOURCE_FILE_BYTES)
        .with_context(|| format!("reading context file {}", path.display()))?;
    String::from_utf8(bytes).map_err(|_| anyhow::anyhow!("context file is not valid UTF-8"))
}

fn absolute_resource_path(path: &Path) -> Result<AbsolutePath> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .context("resolving resource path")?
            .join(path)
    };
    AbsolutePath::parse(path).map_err(|_| anyhow::anyhow!("invalid resource path"))
}

/// Resolves a configured path's components through the capability layer with
/// bounded, identity-pinned managed-config links, returning the fully resolved
/// absolute path (or the lexically-normalized input when no component is a
/// link). Used for global/agent-managed candidate checks.
fn resolve_capability_path(capabilities: &Arc<CapabilityFs>, path: &Path) -> Result<PathBuf> {
    let absolute = absolute_resource_path(path)?;
    capabilities
        .resolve_managed_path_for_read(CapabilityRootPurpose::ProjectResources, absolute)
        .map_err(|error| anyhow::anyhow!("resolving managed link for {}: {error}", path.display()))
}

/// Verifies the source of a platform global/agent-managed context symlink is
/// genuinely operator-controlled agent content, and rejects a resolved target
/// that lands inside the project boundary.
fn authorize_discovered_managed_target(
    capabilities: &Arc<CapabilityFs>,
    path: &Path,
    resolved_target: &Path,
    agent_dir: &Path,
    boundary: &Path,
) -> Result<()> {
    let resolved_agent_dir = resolve_capability_path(capabilities, agent_dir)?;
    let source_parent = path.parent().unwrap_or_else(|| Path::new("/"));
    let resolved_source = resolve_capability_path(capabilities, source_parent)?;
    if !resolved_source.starts_with(&resolved_agent_dir) {
        bail!("context file is a symbolic link whose source is outside the canonical agent dir");
    }
    if resolved_target.starts_with(boundary) {
        bail!("context file is a symbolic link into the project boundary");
    }
    Ok(())
}

/// Reads one context file candidate. `allow_link` selects the link-allowing
/// read for platform global/agent-managed roots; project context candidates
/// are always read strictly (no symlink following).
fn read_context_file_text(
    capabilities: &Arc<CapabilityFs>,
    path: &Path,
    allow_link: bool,
    agent_dir: Option<&Path>,
    boundary: &Path,
) -> Result<String> {
    if allow_link {
        read_global_context_file_text(
            capabilities,
            path,
            agent_dir.expect("global context root for managed link scope"),
            boundary,
        )
    } else {
        read_project_context_file_text(capabilities, path)
    }
}

/// Global context file read: strictly opened first; only a managed-config
/// symlink whose source lives inside the canonical agent dir, whose every hop
/// is identity-pinned by the capability layer, and whose resolved target stays
/// out of the project boundary is followed. The explicit managed root is the
/// canonical target of the discovered entry itself — never the whole user
/// home and never a caller-supplied root list.
fn read_global_context_file_text(
    capabilities: &Arc<CapabilityFs>,
    path: &Path,
    agent_dir: &Path,
    boundary: &Path,
) -> Result<String> {
    let absolute = absolute_resource_path(path)?;
    let managed = capabilities
        .open_managed_read_file(CapabilityRootPurpose::ProjectResources, absolute)
        .with_context(|| format!("opening context file {}", path.display()))?;
    if managed.followed_link() {
        authorize_discovered_managed_target(
            capabilities,
            path,
            managed.resolved_path(),
            agent_dir,
            boundary,
        )?;
    }
    let bytes = managed
        .read_bounded(MAX_RESOURCE_FILE_BYTES)
        .with_context(|| format!("reading context file {}", path.display()))?;
    String::from_utf8(bytes).map_err(|_| anyhow::anyhow!("context file is not valid UTF-8"))
}

/// Read a global context file from a retained agent root lease. The file is
/// opened relative to the lease's retained directory handle, so a root-link
/// retarget after the lease was captured cannot redirect the read to a
/// different directory. Managed-config symlinks within the root are followed
/// through the lease's own managed-entry walker, preserving identity
/// revalidation.
fn read_global_context_file_from_lease(lease: &CapabilityRootLease, name: &str) -> Result<String> {
    let file = match lease.open_file(Path::new(name)) {
        Ok(file) => file,
        Err(error)
            if crate::fs::is_link_blocked(&error)
                || matches!(error, FsError::UnsupportedEntryType) =>
        {
            lease
                .open_file_allowing_link(Path::new(name))
                .with_context(|| format!("opening linked context file {name}"))?
        }
        Err(error) => {
            return Err(error).with_context(|| format!("opening context file {name}"));
        }
    };
    let bytes = file
        .read_bounded(MAX_RESOURCE_FILE_BYTES)
        .with_context(|| format!("reading context file {name}"))?;
    String::from_utf8(bytes).map_err(|_| anyhow::anyhow!("context file is not valid UTF-8"))
}

/// Load global and project context files from retained root leases.
pub(crate) fn load_context_files_from_agent_root_lease(
    capabilities: &Arc<CapabilityFs>,
    agent_root: &CapabilityRootLease,
    project_root: Option<&CapabilityRootLease>,
    cwd: &Path,
    include_project: bool,
) -> Result<Vec<ContextFile>> {
    let cwd = absolute_resource_path(cwd)?.into_path_buf();
    let boundary = project_context_boundary(&cwd)?;
    let mut files = Vec::new();
    let mut seen = Vec::<PathBuf>::new();
    let mut global_diagnostics = Vec::new();
    let mut global_selected = None;
    for name in CONTEXT_FILE_CANDIDATES {
        match read_global_context_file_from_lease(agent_root, name) {
            Ok(content) => {
                if global_selected.is_none() {
                    let path = agent_root.display_root().join(name);
                    global_selected = Some(ContextFile {
                        path: ps(&path),
                        content,
                    });
                }
            }
            Err(error) if error_chain_is_missing(&error) => {}
            Err(error) => global_diagnostics.push(ContextFileDiagnostic {
                path: agent_root.display_root().join(name),
                message: format!("{error:#}"),
            }),
        }
    }
    if !global_diagnostics.is_empty() {
        return Err(ContextFileLoadError {
            diagnostics: global_diagnostics,
        }
        .into());
    }
    if let Some(mut global) = global_selected {
        let raw = PathBuf::from(&global.path);
        global.path = context_display_path(&raw, &boundary, true);
        seen.push(raw);
        files.push(global);
    }
    if !include_project {
        return Ok(files);
    }
    let Some(project_root) = project_root else {
        return Ok(files);
    };
    // A session cwd may sit in a subdirectory of the Git boundary. Walk the
    // ancestor chain from the boundary down to the session directory so
    // subdirectory sessions still receive the boundary-level AGENTS.md /
    // CLAUDE.md context, in the established boundary-first order with the
    // same raw-path dedup as the non-lease loader. Every level is read
    // through a no-follow capability: the retained project lease anchors
    // the session level, and each ancestor level is opened as its own
    // no-follow root. Candidates are never re-opened from an unvalidated
    // path, and a symbolic link anywhere on the chain fails closed.
    let mut levels: Vec<PathBuf> = cwd
        .ancestors()
        .take_while(|ancestor| *ancestor != boundary)
        .map(Path::to_path_buf)
        .collect();
    levels.push(boundary.clone());
    levels.reverse();
    for directory in levels {
        if directory == project_root.display_root() {
            read_project_context_level(project_root, &directory, &boundary, &mut seen, &mut files)?;
        } else {
            let absolute = AbsolutePath::parse(directory.clone())
                .map_err(|_| anyhow::anyhow!("invalid project context directory"))?;
            let lease = capabilities.open_root(
                CapabilityRootPurpose::ProjectResources,
                absolute,
                CapabilityAccess::ReadOnly,
                RootCreation::Existing,
            )?;
            read_project_context_level(&lease, &directory, &boundary, &mut seen, &mut files)?;
        }
    }
    Ok(files)
}

/// Read one project context level's candidates through a no-follow lease
/// rooted at `directory`. The first valid candidate is appended unless its
/// raw path was already seen. Later candidates are still opened: a symlink,
/// oversize read, or invalid UTF-8 fails the level even when an earlier
/// candidate was valid.
fn read_project_context_level(
    lease: &CapabilityRootLease,
    directory: &Path,
    boundary: &Path,
    seen: &mut Vec<PathBuf>,
    files: &mut Vec<ContextFile>,
) -> Result<()> {
    let mut diagnostics = Vec::new();
    let mut selected = false;
    for name in CONTEXT_FILE_CANDIDATES {
        // A project context candidate must never be followed through a
        // symbolic link: the strict no-follow open rejects it with the same
        // explicit diagnostic as the non-lease project reader, failing the
        // load closed instead of reading the link target.
        let raw = directory.join(name);
        let file = match lease.open_file(Path::new(name)) {
            Ok(file) => file,
            Err(error) if crate::fs::is_link_blocked(&error) => {
                diagnostics.push(ContextFileDiagnostic {
                    path: raw,
                    message: "context file is a symbolic link".to_owned(),
                });
                continue;
            }
            Err(FsError::System {
                kind: std::io::ErrorKind::NotFound,
                ..
            }) => continue,
            Err(error) => return Err(error.into()),
        };
        let bytes = match file.read_bounded(MAX_RESOURCE_FILE_BYTES) {
            Ok(bytes) => bytes,
            Err(error) => {
                diagnostics.push(ContextFileDiagnostic {
                    path: raw,
                    message: format!("{error:#}"),
                });
                continue;
            }
        };
        let content = match String::from_utf8(bytes) {
            Ok(content) => content,
            Err(_) => {
                diagnostics.push(ContextFileDiagnostic {
                    path: raw,
                    message: "context file is not valid UTF-8".to_owned(),
                });
                continue;
            }
        };
        if !selected {
            // Already recorded from the global root or an earlier level.
            // That file is still the winning candidate, so a later sibling
            // such as CLAUDE.md must not be appended.
            if !seen.iter().any(|seen_path| seen_path == &raw) {
                seen.push(raw.clone());
                files.push(ContextFile {
                    path: context_display_path(&raw, boundary, false),
                    content,
                });
            }
            selected = true;
        }
    }
    if !diagnostics.is_empty() {
        return Err(ContextFileLoadError { diagnostics }.into());
    }
    Ok(())
}

pub(crate) fn strip_skill_frontmatter(content: &str) -> String {
    let normalized = content.replace("\r\n", "\n").replace('\r', "\n");
    normalized
        .strip_prefix("---\n")
        .and_then(|rest| rest.split_once("\n---"))
        .map_or_else(
            || normalized.clone(),
            |(_, body)| body.trim_start_matches("\n---").trim_start().to_owned(),
        )
}

/// Max name/description lengths per the Agent Skills spec (skills.ts:11,14).
const MAX_SKILL_NAME_LENGTH: i64 = 64;
const MAX_SKILL_DESCRIPTION_LENGTH: i64 = 1024;

const CONTEXT_FILE_CANDIDATES: &[&str] = &["AGENTS.md", "AGENTS.MD", "CLAUDE.md", "CLAUDE.MD"];
const SKILL_IGNORE_FILE_NAMES: &[&str] = &[".gitignore", ".ignore", ".fdignore"];

/// The active config profile for this process, installed once by the CLI
/// before any dispatch (`--profile` wins over `PI_PROFILE`). `None` means the
/// default (unprofiled) user base is active.
static ACTIVE_PROFILE: OnceLock<Option<String>> = OnceLock::new();

/// Set the active config profile for this process. `None` selects the default
/// (unprofiled) user base; a named profile relocates every agent-dir-derived
/// path under `<base>/profiles/<name>`. The profile is a process constant
/// fixed at startup, so a second call is ignored.
pub fn set_active_profile(profile: Option<&str>) {
    let _ = ACTIVE_PROFILE.set(profile.map(str::to_owned));
}

/// Relocate a user base directory under the active config profile:
/// `<base>` becomes `<base>/profiles/<name>` when a named profile is active,
/// and is returned unchanged for the default profile.
#[must_use]
pub fn apply_profile(base: PathBuf) -> PathBuf {
    match ACTIVE_PROFILE.get().and_then(|profile| profile.as_deref()) {
        Some(name) => base.join("profiles").join(name),
        None => base,
    }
}

/// The rpi user directory (`~/.rpi`), honoring the agent/config precedence
/// `PI_CODING_AGENT_DIR` > `RPI_HOME` > `HOME/.rpi`, before any profile
/// relocation. Shared by [`agent_dir`], the session store, and the session
/// catalog so all apply the active profile exactly once.
pub(crate) fn agent_dir_base() -> PathBuf {
    let home = home_dir().map(PathBuf::from);
    #[cfg(test)]
    if HOME_OVERRIDE.with(|override_home| override_home.borrow().is_some()) {
        return home.as_ref().map_or_else(
            || PathBuf::from(CONFIG_DIR_NAME),
            |home| home.join(CONFIG_DIR_NAME),
        );
    }
    resolve_agent_dir_base(
        std::env::var_os("PI_CODING_AGENT_DIR").as_deref(),
        std::env::var_os("RPI_HOME").as_deref(),
        home.as_deref(),
    )
}

/// Pure resolver for [`agent_dir_base`], factored out so the
/// `PI_CODING_AGENT_DIR` > `RPI_HOME` > `HOME/.rpi` precedence is unit-testable
/// without mutating the process environment. `~` expands against `user_home`
/// and relative values become absolute via the current directory.
fn resolve_agent_dir_base(
    agent_dir: Option<&std::ffi::OsStr>,
    rpi_home: Option<&std::ffi::OsStr>,
    user_home: Option<&Path>,
) -> PathBuf {
    let fallback = user_home.map_or_else(
        || PathBuf::from(CONFIG_DIR_NAME),
        |home| home.join(CONFIG_DIR_NAME),
    );
    let Some(configured) = agent_dir
        .filter(|value| !value.is_empty())
        .or_else(|| rpi_home.filter(|value| !value.is_empty()))
    else {
        return fallback;
    };
    let configured = PathBuf::from(configured);
    let expanded = if configured == Path::new("~") {
        user_home.map(Path::to_path_buf).unwrap_or(configured)
    } else if let Ok(rest) = configured.strip_prefix("~") {
        user_home.map_or(configured.clone(), |home| home.join(rest))
    } else {
        configured
    };
    if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir().map_or(expanded.clone(), |cwd| cwd.join(expanded))
    }
}

/// Returns the global rpi directory (`~/.rpi`), relocated under
/// the active config profile (`<base>/profiles/<name>`) when one is active.
pub fn agent_dir() -> String {
    ps(&apply_profile(agent_dir_base()))
}

#[must_use]
pub fn agent_dir_path() -> PathBuf {
    PathBuf::from(agent_dir())
}

/// Best-effort home directory (HOME on Unix, USERPROFILE on Windows), mirroring
/// Go's `os.UserHomeDir`.
pub(crate) fn home_dir() -> Option<String> {
    // Test-only override (parallel-safe, no global env mutation).
    HOME_OVERRIDE
        .with(|o| o.borrow().clone())
        .or_else(|| env_home("HOME"))
        .or_else(|| env_home("USERPROFILE"))
}

fn env_home(var: &str) -> Option<String> {
    let h = std::env::var(var).ok()?;
    if h.is_empty() { None } else { Some(h) }
}

thread_local! {
    static HOME_OVERRIDE: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn with_home_override<T>(home: &Path, operation: impl FnOnce() -> T) -> T {
    HOME_OVERRIDE.with(|override_home| override_home.replace(Some(home.to_string_lossy().into_owned())));
    let value = operation();
    HOME_OVERRIDE.with(|override_home| override_home.take());
    value
}

pub(crate) fn has_git_context_boundary(cwd: impl AsRef<Path>) -> Result<bool> {
    let cwd = absolute_resource_path(cwd.as_ref())?.into_path_buf();
    Ok(find_git_paths(&cwd)?.is_some())
}

/// The pi package root directory: honor `PI_PACKAGE_DIR`, else walk up from the
/// executable until a `package.json` is found, else fall back to the exe dir.
pub fn package_dir() -> String {
    if let Ok(env) = std::env::var("PI_PACKAGE_DIR") {
        if !env.is_empty() {
            return env;
        }
    }
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(_) => return ".".to_string(),
    };
    let mut dir = exe
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    loop {
        let has_package = CapabilityFs::new()
            .ok()
            .and_then(|capabilities| {
                let absolute = AbsolutePath::parse(dir.clone()).ok()?;
                let lease = capabilities
                    .open_root(
                        CapabilityRootPurpose::ProjectResources,
                        absolute,
                        CapabilityAccess::ReadOnly,
                        RootCreation::Existing,
                    )
                    .ok()?;
                lease.open_file(Path::new("package.json")).ok()
            })
            .is_some();
        if has_package {
            return ps(&dir);
        }
        let parent = match dir.parent() {
            Some(p) => p.to_path_buf(),
            None => break,
        };
        if parent == dir {
            break;
        }
        dir = parent;
    }
    exe.parent()
        .map(|p| ps(p))
        .unwrap_or_else(|| ".".to_string())
}

pub fn readme_path() -> String {
    abs_join(&package_dir(), "README.md")
}

pub fn docs_path() -> String {
    abs_join(&package_dir(), "docs")
}

pub fn examples_path() -> String {
    abs_join(&package_dir(), "examples")
}

fn abs_join(base: &str, rel: &str) -> String {
    let p = Path::new(base).join(rel);
    // Make absolute relative to cwd if needed (filepath.Abs).
    abs_path(&p)
}

fn abs_path(p: &Path) -> String {
    if p.is_absolute() {
        clean_path(p)
    } else {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        clean_path(&cwd.join(p))
    }
}

/// `filepath.Clean`: normalize `.`, `..`, doubled separators.
fn clean_path(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    let mut out: Vec<&str> = Vec::new();
    let mut leading_slash = false;
    for (i, comp) in s.split('/').enumerate() {
        if i == 0 && comp.is_empty() {
            leading_slash = true;
            continue;
        }
        match comp {
            "" | "." => continue,
            ".." => {
                if let Some(last) = out.last() {
                    if *last != ".." {
                        out.pop();
                        continue;
                    }
                }
                out.push("..");
            }
            other => out.push(other),
        }
    }
    let joined = out.join("/");
    match (leading_slash, joined.is_empty()) {
        (true, _) => format!("/{joined}"),
        (false, true) => ".".to_string(),
        (false, false) => joined,
    }
}

fn load_context_file_from_dir(
    capabilities: &Arc<CapabilityFs>,
    dir: &Path,
    allow_link: bool,
    agent_dir: Option<&Path>,
    boundary: &Path,
) -> std::result::Result<Option<ContextFile>, ContextFileLoadError> {
    let mut selected = None;
    let mut diagnostics = Vec::new();
    for name in CONTEXT_FILE_CANDIDATES {
        let path = dir.join(name);
        match read_context_file_text(capabilities, &path, allow_link, agent_dir, boundary) {
            Ok(content) => {
                if selected.is_none() {
                    selected = Some(ContextFile {
                        path: ps(&path),
                        content,
                    });
                }
            }
            Err(error) if error_chain_is_missing(&error) => {}
            // Surface the full error chain (`{:#}`) so the diagnostic names
            // the real cause — e.g. the actual byte limit for an oversize
            // candidate or the UTF-8 decode failure — not just the top context.
            Err(error) => diagnostics.push(ContextFileDiagnostic {
                path,
                message: format!("{error:#}"),
            }),
        }
    }
    if diagnostics.is_empty() {
        Ok(selected)
    } else {
        Err(ContextFileLoadError { diagnostics })
    }
}

fn error_chain_is_missing(error: &anyhow::Error) -> bool {
    error.chain().any(|source| {
        source.downcast_ref::<FsError>().is_some_and(|error| {
            matches!(
                error,
                FsError::System {
                    kind: std::io::ErrorKind::NotFound,
                    ..
                }
            )
        })
    })
}

#[derive(Clone)]
struct GitPaths {
    repo_dir: PathBuf,
}

/// Finds the nearest Git worktree root. A `.git` entry is a security boundary:
fn find_git_paths(cwd: &Path) -> Result<Option<GitPaths>> {
    let capabilities = CapabilityFs::new()?;
    let mut directory = cwd.to_path_buf();
    loop {
        let absolute = AbsolutePath::parse(directory.clone())
            .map_err(|_| anyhow::anyhow!("invalid project directory"))?;
        let lease = capabilities.open_root(
            CapabilityRootPurpose::ProjectResources,
            absolute,
            CapabilityAccess::ReadOnly,
            RootCreation::Existing,
        )?;
        match lease.open_directory(Path::new(".git")) {
            Ok(directory_entry) => {
                let git = directory_entry.display_path();
                validate_git_head(&capabilities, &git)?;
                return Ok(Some(GitPaths {
                    repo_dir: directory,
                }));
            }
            // A regular-file `.git` (gitdir pointer) cannot be opened as a
            // directory: Linux/macOS surface ENOTDIR, other platforms may
            // classify the entry as unsupported. Either way, parse the
            // `gitdir:` control file. Symlinked `.git` entries never reach
            // this arm — the capability layer refuses to follow symlinks — and
            // fall through to the rejecting error below.
            Err(error)
                if matches!(error, FsError::UnsupportedEntryType)
                    || matches!(
                        error,
                        FsError::System {
                            kind: std::io::ErrorKind::NotADirectory,
                            ..
                        }
                    ) =>
            {
                let file = lease.open_file(Path::new(".git"))?;
                let content = String::from_utf8(file.read_bounded(64 * 1024)?)
                    .context("decoding Git control file")?;
                let git_dir = content
                    .trim()
                    .strip_prefix("gitdir: ")
                    .filter(|path| !path.trim().is_empty())
                    .ok_or_else(|| anyhow::anyhow!("invalid gitdir pointer"))?;
                let git_dir = if Path::new(git_dir).is_absolute() {
                    PathBuf::from(git_dir)
                } else {
                    directory.join(git_dir)
                };
                // Resolve `.`/`..` lexically (git submodule pointers are
                // commonly relative) without ambient `canonicalize`: the
                // capability layer still refuses to follow symlinks when
                // opening the resulting HEAD.
                validate_git_head(&capabilities, &lexically_normalize(git_dir))?;
                return Ok(Some(GitPaths {
                    repo_dir: directory,
                }));
            }
            Err(error)
                if matches!(
                    error,
                    FsError::System {
                        kind: std::io::ErrorKind::NotFound,
                        ..
                    }
                ) => {}
            Err(error) => return Err(error).context("inspecting Git boundary"),
        }
        let Some(parent) = directory.parent().map(Path::to_path_buf) else {
            return Ok(None);
        };
        if parent == directory {
            return Ok(None);
        }
        directory = parent;
    }
}

fn validate_git_head(capabilities: &Arc<CapabilityFs>, git_dir: &Path) -> Result<()> {
    let head = git_dir.join("HEAD");
    capabilities
        .open_exact_file(
            CapabilityRootPurpose::ProjectResources,
            absolute_resource_path(&head)?,
        )
        .with_context(|| format!("opening Git HEAD {}", head.display()))?;
    Ok(())
}

/// Resolves `.`/`..` components lexically without touching the filesystem, so
/// the result is always a normalized absolute path the capability layer can
/// open without following symlinks. `/..` at the root stays at the root.
fn lexically_normalize(path: PathBuf) -> PathBuf {
    use std::path::Component;
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if normalized.file_name().is_some() {
                    normalized.pop();
                }
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn parent_dir(path: &str) -> String {
    Path::new(path)
        .parent()
        .map(ps)
        .unwrap_or_else(|| path.to_owned())
}

fn basename(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Returns the project context boundary: the nearest Git worktree root
/// containing `cwd`, or `cwd` itself outside Git.
pub fn project_context_boundary(cwd: impl AsRef<Path>) -> Result<PathBuf> {
    let cwd = absolute_resource_path(cwd.as_ref())?.into_path_buf();
    let capabilities = CapabilityFs::new()?;
    capabilities.open_root(
        CapabilityRootPurpose::ProjectResources,
        AbsolutePath::parse(cwd.clone())
            .map_err(|_| anyhow::anyhow!("invalid working directory"))?,
        CapabilityAccess::ReadOnly,
        RootCreation::Existing,
    )?;
    Ok(find_git_paths(&cwd)?.map_or(cwd, |paths| paths.repo_dir))
}

fn context_display_path(path: &Path, boundary: &Path, is_global: bool) -> String {
    if is_global {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("CONTEXT.md");
        return format!("<agent-dir>/{name}");
    }
    path.strip_prefix(boundary)
        .map(|relative| {
            let relative = relative.to_string_lossy().replace('\\', "/");
            if relative.is_empty() {
                ".".to_owned()
            } else {
                relative
            }
        })
        .unwrap_or_else(|_| {
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("CONTEXT.md")
                .to_owned()
        })
}

pub fn load_context_files_from_agent_dir(
    cwd: &Path,
    agent_dir: &Path,
    include_project: bool,
) -> Result<Vec<ContextFile>> {
    let capabilities = CapabilityFs::new()?;
    load_context_files_from_agent_dir_with_capabilities(
        &capabilities,
        cwd,
        agent_dir,
        include_project,
    )
}

pub(crate) fn load_context_files_from_agent_dir_with_capabilities(
    capabilities: &Arc<CapabilityFs>,
    cwd: &Path,
    agent_dir: &Path,
    include_project: bool,
) -> Result<Vec<ContextFile>> {
    let cwd = absolute_resource_path(cwd)?.into_path_buf();
    let boundary = project_context_boundary(&cwd)?;
    let mut files = Vec::new();
    let mut seen = Vec::<PathBuf>::new();
    // The platform global/agent-managed root may resolve managed-config
    // symlinks, but only within the canonical agent-dir / owner-home managed
    // roots and never into the project boundary; every project context
    // candidate stays strict (nofollow).
    if let Some(mut global) =
        load_context_file_from_dir(capabilities, agent_dir, true, Some(agent_dir), &boundary)?
    {
        let raw = PathBuf::from(&global.path);
        global.path = context_display_path(&raw, &boundary, true);
        seen.push(raw);
        files.push(global);
    }
    if !include_project {
        return Ok(files);
    }
    let mut ancestors = cwd
        .ancestors()
        .take_while(|ancestor| *ancestor != boundary)
        .map(Path::to_path_buf)
        .collect::<Vec<_>>();
    ancestors.push(boundary.clone());
    ancestors.reverse();
    for directory in ancestors {
        let Some(mut context) =
            load_context_file_from_dir(capabilities, &directory, false, None, &boundary)?
        else {
            continue;
        };
        let raw = PathBuf::from(&context.path);
        if seen.iter().any(|seen_path| seen_path == &raw) {
            continue;
        }
        context.path = context_display_path(&raw, &boundary, false);
        seen.push(raw);
        files.push(context);
    }
    Ok(files)
}

pub fn load_context_files(cwd: &str, include_project: bool) -> Result<Vec<ContextFile>> {
    load_context_files_from_agent_dir(Path::new(cwd), &agent_dir_path(), include_project)
}

pub fn load_project_context_files(cwd: &str) -> Result<Vec<ContextFile>> {
    load_context_files(cwd, true)
}

pub fn load_skills(cwd: &str) -> Vec<Skill> {
    load_skills_trusted(cwd, true).0
}

pub fn load_skills_with_diagnostics(cwd: &str) -> (Vec<Skill>, Vec<SkillDiagnostic>) {
    load_skills_trusted(cwd, true)
}

pub(crate) fn load_skills_trusted_from_agent_dir(
    cwd: &str,
    agent_dir: &Path,
    include_project: bool,
) -> (Vec<Skill>, Vec<SkillDiagnostic>) {
    load_skills_trusted_from_dirs(cwd, agent_dir, include_project)
}

pub fn load_skills_trusted(cwd: &str, include_project: bool) -> (Vec<Skill>, Vec<SkillDiagnostic>) {
    load_skills_trusted_from_dirs(cwd, Path::new(&agent_dir()), include_project)
}

fn load_skills_trusted_from_dirs(
    cwd: &str,
    agent_dir: &Path,
    include_project: bool,
) -> (Vec<Skill>, Vec<SkillDiagnostic>) {
    let mut skills = Vec::new();
    let mut diagnostics = Vec::new();
    let mut seen = Vec::new();
    let mut add = |found: Vec<Skill>, found_diagnostics: Vec<SkillDiagnostic>| {
        diagnostics.extend(found_diagnostics);
        for skill in found {
            if seen.contains(&skill.name) {
                continue;
            }
            seen.push(skill.name.clone());
            skills.push(skill);
        }
    };
    let (global, global_diagnostics) =
        load_skills_from_dir(&ps(agent_dir.join("skills")), SkillSource::User, true, true);
    add(global, global_diagnostics);
    if include_project {
        let (project, project_diagnostics) = load_skills_from_dir(
            &ps(Path::new(cwd).join(CONFIG_DIR_NAME).join("skills")),
            SkillSource::Project,
            true,
            false,
        );
        add(project, project_diagnostics);
    }
    (skills, diagnostics)
}

// ---------------------------------------------------------------------------
// Skills
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SkillSource {
    User,
    Project,
    PackageGlobal,
    PackageProject,
    Explicit,
}

impl SkillSource {
    #[must_use]
    pub const fn precedence(self) -> u8 {
        match self {
            Self::User => 50,
            Self::Project => 40,
            Self::PackageGlobal => 30,
            Self::PackageProject => 20,
            Self::Explicit => 10,
        }
    }

    #[must_use]
    pub(crate) const fn allows_managed_links(self) -> bool {
        matches!(self, Self::User | Self::PackageGlobal)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub file_path: String,
    pub base_dir: String,
    pub globs: Vec<String>,
    pub always_apply: bool,
    pub hidden: bool,
    pub disable_model_invocation: bool,
    pub source: SkillSource,
    pub managed_project_boundary: Option<PathBuf>,
    pub managed_root: Option<PathBuf>,
    pub trusted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillDiagnostic {
    /// `"warning"` | `"error"`.
    pub kind: String,
    pub message: String,
    pub path: String,
}

/// Scans a directory for skills. Discovery rules:
/// - a directory containing `SKILL.md` is a skill root (no further recursion);
/// - otherwise load direct `.md` children of the root, and recurse into
///   subdirectories looking for `SKILL.md`;
/// - honor `.gitignore`/`.ignore`/`.fdignore`, skip `node_modules`, follow
///   symlinks but realpath-dedup so a symlink loop or duplicate target is visited
///   once.
pub(crate) fn load_skills_from_dir(
    dir: &str,
    source: SkillSource,
    trusted: bool,
    allow_link: bool,
) -> (Vec<Skill>, Vec<SkillDiagnostic>) {
    let Ok(capabilities) = CapabilityFs::new() else {
        return (Vec::new(), Vec::new());
    };
    load_skills_from_dir_with_capabilities(&capabilities, dir, source, trusted, allow_link)
}

pub(crate) fn load_skills_from_dir_with_capabilities(
    capabilities: &Arc<CapabilityFs>,
    dir: &str,
    source: SkillSource,
    trusted: bool,
    allow_link: bool,
) -> (Vec<Skill>, Vec<SkillDiagnostic>) {
    load_skills_from_dir_with_capabilities_and_boundary(
        capabilities,
        dir,
        source,
        trusted,
        allow_link,
        None,
    )
}

pub(crate) fn load_skills_from_dir_with_capabilities_and_boundary(
    capabilities: &Arc<CapabilityFs>,
    dir: &str,
    source: SkillSource,
    trusted: bool,
    allow_link: bool,
    managed_project_boundary: Option<&Path>,
) -> (Vec<Skill>, Vec<SkillDiagnostic>) {
    let root = PathBuf::from(dir);
    let Ok(absolute) = AbsolutePath::parse(root.clone()) else {
        return (Vec::new(), Vec::new());
    };
    let lease = if allow_link && source.allows_managed_links() {
        match capabilities
            .open_read_directory_allowing_link(CapabilityRootPurpose::AgentStore, absolute.clone())
        {
            Ok(lease) => lease,
            Err(error)
                if matches!(
                    error,
                    FsError::System {
                        kind: std::io::ErrorKind::NotFound,
                        ..
                    }
                ) =>
            {
                let error = match capabilities.open_root(
                    CapabilityRootPurpose::AgentStore,
                    absolute,
                    CapabilityAccess::ReadOnly,
                    RootCreation::Existing,
                ) {
                    Err(strict_error)
                        if matches!(
                            strict_error,
                            FsError::System {
                                kind: std::io::ErrorKind::NotFound,
                                ..
                            }
                        ) =>
                    {
                        return (Vec::new(), Vec::new());
                    }
                    Err(strict_error) => strict_error,
                    Ok(_) => error,
                };
                return (
                    Vec::new(),
                    vec![SkillDiagnostic {
                        kind: "error".to_owned(),
                        message: error.to_string(),
                        path: dir.to_owned(),
                    }],
                );
            }
            Err(error) => {
                return (
                    Vec::new(),
                    vec![SkillDiagnostic {
                        kind: "error".to_owned(),
                        message: error.to_string(),
                        path: dir.to_owned(),
                    }],
                );
            }
        }
    } else {
        match capabilities.open_root(
            CapabilityRootPurpose::ProjectResources,
            absolute,
            CapabilityAccess::ReadOnly,
            RootCreation::Existing,
        ) {
            Ok(lease) => lease,
            Err(_) => return (Vec::new(), Vec::new()),
        }
    };
    let mut ignore = SkillIgnore::new();
    let (mut skills, diagnostics) = load_skills_from_lease(
        &lease,
        Path::new(""),
        true,
        source,
        trusted,
        managed_project_boundary,
        &mut ignore,
    );
    for skill in &mut skills {
        if skill.managed_project_boundary.is_none() {
            skill.managed_project_boundary = managed_project_boundary.map(Path::to_path_buf);
        }
        if skill.managed_root.is_none() && source.allows_managed_links() {
            skill.managed_root = Some(lease.authority_root().to_path_buf());
        }
    }
    (skills, diagnostics)
}

/// Load skills from a retained agent root lease. The lease is the same one
/// captured by `build_candidate` for the configured AgentStore root, so a
/// root-link retarget after capture cannot redirect skill discovery to a
/// different directory. The `subdir` (e.g. `skills`) is opened relative to
/// the lease's retained handle, following managed-config links within the
/// root with identity revalidation.
pub(crate) fn load_skills_from_agent_root_lease(
    agent_root: &CapabilityRootLease,
    subdir: &Path,
    source: SkillSource,
    trusted: bool,
    managed_project_boundary: Option<&Path>,
) -> (Vec<Skill>, Vec<SkillDiagnostic>) {
    let skills_directory = match agent_root.open_directory_allowing_link(subdir) {
        Ok(directory) => directory,
        Err(error)
            if matches!(
                error,
                FsError::System {
                    kind: std::io::ErrorKind::NotFound,
                    ..
                }
            ) =>
        {
            let error = match agent_root.open_directory(subdir) {
                Err(strict_error)
                    if matches!(
                        strict_error,
                        FsError::System {
                            kind: std::io::ErrorKind::NotFound,
                            ..
                        }
                    ) =>
                {
                    return (Vec::new(), Vec::new());
                }
                Err(strict_error) => strict_error,
                Ok(_) => error,
            };
            return (
                Vec::new(),
                vec![SkillDiagnostic {
                    kind: "error".to_owned(),
                    message: error.to_string(),
                    path: ps(agent_root.display_root().join(subdir)),
                }],
            );
        }
        Err(error) => {
            return (
                Vec::new(),
                vec![SkillDiagnostic {
                    kind: "error".to_owned(),
                    message: error.to_string(),
                    path: ps(agent_root.display_root().join(subdir)),
                }],
            );
        }
    };
    let skills_lease = match skills_directory.into_root_lease() {
        Ok(lease) => lease,
        Err(error)
            if matches!(
                error,
                FsError::System {
                    kind: std::io::ErrorKind::NotFound,
                    ..
                }
            ) =>
        {
            return (Vec::new(), Vec::new());
        }
        Err(error) => {
            return (
                Vec::new(),
                vec![SkillDiagnostic {
                    kind: "error".to_owned(),
                    message: error.to_string(),
                    path: ps(agent_root.display_root().join(subdir)),
                }],
            );
        }
    };
    if source.allows_managed_links()
        && managed_target_crosses_project_boundary(
            skills_lease.authority_root(),
            skills_lease.display_root(),
            managed_project_boundary,
        )
    {
        return (
            Vec::new(),
            vec![SkillDiagnostic {
                kind: "warning".to_owned(),
                message: "managed skill link crosses the current project boundary".to_owned(),
                path: ps(agent_root.display_root().join(subdir)),
            }],
        );
    }
    let mut ignore = SkillIgnore::new();
    let (mut skills, diagnostics) = load_skills_from_lease(
        &skills_lease,
        Path::new(""),
        true,
        source,
        trusted,
        managed_project_boundary,
        &mut ignore,
    );
    for skill in &mut skills {
        if skill.managed_project_boundary.is_none() {
            skill.managed_project_boundary = managed_project_boundary.map(Path::to_path_buf);
        }
        if skill.managed_root.is_none() && source.allows_managed_links() {
            skill.managed_root = Some(skills_lease.authority_root().to_path_buf());
        }
    }
    (skills, diagnostics)
}

fn load_skills_from_lease(
    lease: &crate::fs::CapabilityRootLease,
    relative: &Path,
    include_root_files: bool,
    source: SkillSource,
    trusted: bool,
    managed_project_boundary: Option<&Path>,
    ignore: &mut SkillIgnore,
) -> (Vec<Skill>, Vec<SkillDiagnostic>) {
    let mut skills = Vec::new();
    let mut diagnostics = Vec::new();
    let directory = match open_skill_directory(lease, relative, source, managed_project_boundary) {
        Ok(directory) => directory,
        Err(diagnostic) => {
            diagnostics.push(diagnostic);
            return (skills, diagnostics);
        }
    };
    ignore.add_rules_from_directory(&directory, relative);
    let names = match directory.read_dir_names() {
        Ok(names) => names,
        Err(error) => {
            diagnostics.push(skill_entry_diagnostic(lease, relative, source, error));
            return (skills, diagnostics);
        }
    };
    if names.iter().any(|name| name == "SKILL.md") {
        let child = relative.join("SKILL.md");
        let rel = to_posix(&ps(&child));
        if !ignore.ignores(&rel, false) {
            match open_skill_entry(
                lease,
                &directory,
                &child,
                Path::new("SKILL.md"),
                source,
                managed_project_boundary,
            ) {
                Ok((CapabilityEntry::File(file), path)) => {
                    let (skill, found) =
                        load_skill_from_capability_file_at(file, &path, source, trusted);
                    diagnostics.extend(found);
                    if let Some(skill) = skill {
                        skills.push(skill);
                    }
                }
                Ok((CapabilityEntry::Directory(_), _)) => diagnostics.push(SkillDiagnostic {
                    kind: "error".to_owned(),
                    message: "skill entry SKILL.md is not a regular file".to_owned(),
                    path: ps(&lease.display_root().join(&child)),
                }),
                Err(diagnostic) => diagnostics.push(diagnostic),
            }
        }
        return (skills, diagnostics);
    }
    for name in names {
        let name_string = name.to_string_lossy().into_owned();
        if name_string.starts_with('.') || name_string == "node_modules" {
            continue;
        }
        let child = relative.join(&name);
        match open_skill_entry(
            lease,
            &directory,
            &child,
            Path::new(&name),
            source,
            managed_project_boundary,
        ) {
            Ok((CapabilityEntry::Directory(_), _)) => {
                let rel = format!("{}/", to_posix(&ps(&child)));
                if ignore.ignores(&rel, true) {
                    continue;
                }
                let (nested, found) = load_skills_from_lease(
                    lease,
                    &child,
                    false,
                    source,
                    trusted,
                    managed_project_boundary,
                    ignore,
                );
                skills.extend(nested);
                diagnostics.extend(found);
            }
            Ok((CapabilityEntry::File(file), path)) => {
                if !include_root_files
                    || !name_string.ends_with(".md")
                    || ignore.ignores(&to_posix(&ps(&child)), false)
                {
                    continue;
                }
                let (skill, found) =
                    load_skill_from_capability_file_at(file, &path, source, trusted);
                diagnostics.extend(found);
                if let Some(skill) = skill {
                    skills.push(skill);
                }
            }
            Err(diagnostic) => diagnostics.push(diagnostic),
        }
    }
    (skills, diagnostics)
}

fn open_skill_directory(
    lease: &crate::fs::CapabilityRootLease,
    relative: &Path,
    source: SkillSource,
    managed_project_boundary: Option<&Path>,
) -> std::result::Result<crate::fs::CapabilityDirectory, SkillDiagnostic> {
    if relative.as_os_str().is_empty() {
        return lease
            .open_directory(Path::new(""))
            .map_err(|error| skill_entry_diagnostic(lease, relative, source, error));
    }
    let root_directory = lease
        .open_directory(Path::new(""))
        .map_err(|error| skill_entry_diagnostic(lease, relative, source, error))?;
    match open_skill_entry(
        lease,
        &root_directory,
        relative,
        relative,
        source,
        managed_project_boundary,
    )? {
        (CapabilityEntry::Directory(directory), _) => Ok(directory),
        (CapabilityEntry::File(_), _) => Err(SkillDiagnostic {
            kind: "error".to_owned(),
            message: "skill path is not a directory".to_owned(),
            path: ps(&lease.display_root().join(relative)),
        }),
    }
}

fn managed_target_crosses_project_boundary(
    resolved: &Path,
    display_root: &Path,
    boundary: Option<&Path>,
) -> bool {
    // Inside the project, but not under this skill root's display path.
    boundary.is_some_and(|boundary| {
        resolved.starts_with(boundary) && !resolved.starts_with(display_root)
    })
}

fn open_skill_entry(
    lease: &crate::fs::CapabilityRootLease,
    directory: &crate::fs::CapabilityDirectory,
    child: &Path,
    directory_relative: &Path,
    source: SkillSource,
    managed_project_boundary: Option<&Path>,
) -> std::result::Result<(CapabilityEntry, PathBuf), SkillDiagnostic> {
    if source.allows_managed_links() {
        let opened = lease
            .open_managed_entry_within_root(child)
            .map_err(|error| skill_entry_diagnostic(lease, child, source, error))?;
        if managed_target_crosses_project_boundary(
            opened.resolved_path(),
            lease.display_root(),
            managed_project_boundary,
        ) {
            return Err(SkillDiagnostic {
                kind: "warning".to_owned(),
                message: "managed skill link crosses the current project boundary".to_owned(),
                path: ps(&lease.display_root().join(child)),
            });
        }
        // The resolved path comes from the same retained open that produced the
        // entry handle, so the caller never needs a second open or an
        // unvalidated display-path fallback.
        let resolved = opened.resolved_path().to_path_buf();
        return Ok((opened.into_entry(), resolved));
    }
    let entry = directory
        .open_entry_exact(directory_relative)
        .map_err(|error| skill_entry_diagnostic(lease, child, source, error))?;
    // No link following for strict sources: the entry's display path is the
    // authoritative path it was opened at.
    let resolved = entry.display_path();
    Ok((entry, resolved))
}

fn skill_entry_diagnostic(
    lease: &crate::fs::CapabilityRootLease,
    relative: &Path,
    source: SkillSource,
    error: FsError,
) -> SkillDiagnostic {
    let message = if source.allows_managed_links() && matches!(error, FsError::OutsideCapability) {
        "managed skill link crosses the project boundary or escapes its controlled source root"
            .to_owned()
    } else if !source.allows_managed_links()
        && (crate::fs::is_link_blocked(&error) || matches!(error, FsError::UnsupportedEntryType))
    {
        "symbolic links are not allowed for this skill source".to_owned()
    } else {
        error.to_string()
    };
    SkillDiagnostic {
        kind: "error".to_owned(),
        message,
        path: ps(&lease.display_root().join(relative)),
    }
}

/// Parses one skill markdown file. The skill loads even with name/description
/// warnings, except when description is missing entirely.
pub(crate) fn load_skill_from_file(
    file_path: &str,
    source: SkillSource,
    trusted: bool,
) -> (Option<Skill>, Vec<SkillDiagnostic>) {
    let path = Path::new(file_path);
    match read_resource_text(path, "skill") {
        Ok(data) => load_skill_from_text_for_resource_manager(path, &data, source, trusted),
        Err(error) => (
            None,
            vec![SkillDiagnostic {
                kind: "warning".to_owned(),
                message: error.to_string(),
                path: file_path.to_owned(),
            }],
        ),
    }
}

fn load_skill_from_capability_file(
    file: crate::fs::CapabilityFile,
    source: SkillSource,
    trusted: bool,
) -> (Option<Skill>, Vec<SkillDiagnostic>) {
    let path = file.display_path();
    load_skill_from_capability_file_at(file, &path, source, trusted)
}

fn load_skill_from_capability_file_at(
    file: crate::fs::CapabilityFile,
    path: &Path,
    source: SkillSource,
    trusted: bool,
) -> (Option<Skill>, Vec<SkillDiagnostic>) {
    match file.read_bounded(MAX_RESOURCE_FILE_BYTES) {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(data) => load_skill_from_text_for_resource_manager(path, &data, source, trusted),
            Err(error) => (
                None,
                vec![SkillDiagnostic {
                    kind: "warning".to_owned(),
                    message: format!("skill is not valid UTF-8: {error}"),
                    path: ps(path),
                }],
            ),
        },
        Err(error) => (
            None,
            vec![SkillDiagnostic {
                kind: "warning".to_owned(),
                message: error.to_string(),
                path: ps(path),
            }],
        ),
    }
}

pub(crate) fn load_skill_from_text_for_resource_manager(
    path: &Path,
    data: &str,
    source: SkillSource,
    trusted: bool,
) -> (Option<Skill>, Vec<SkillDiagnostic>) {
    let file_path = ps(path);
    let mut diags = Vec::new();
    let (fm, _) = parse_frontmatter(data);
    let skill_dir = parent_dir(&file_path);
    let desc = fm
        .get("description")
        .map(|value| value.value.clone())
        .unwrap_or_default();
    for message in validate_description(&desc) {
        diags.push(SkillDiagnostic {
            kind: "warning".to_owned(),
            message,
            path: file_path.clone(),
        });
    }
    let mut name = fm
        .get("name")
        .map(|value| value.value.clone())
        .unwrap_or_default();
    if name.is_empty() {
        name = basename(&skill_dir);
    }
    for message in validate_name(&name) {
        diags.push(SkillDiagnostic {
            kind: "warning".to_owned(),
            message,
            path: file_path.clone(),
        });
    }
    if desc.trim().is_empty() {
        return (None, diags);
    }
    let managed_project_boundary = None;
    (
        Some(Skill {
            name,
            description: desc,
            file_path,
            base_dir: skill_dir,
            globs: fm
                .get("globs")
                .map(parse_frontmatter_list)
                .unwrap_or_default(),
            always_apply: fm
                .get("alwaysApply")
                .or_else(|| fm.get("always-apply"))
                .is_some_and(FmValue::is_bool_true),
            hidden: fm
                .get("hide")
                .or_else(|| fm.get("hidden"))
                .is_some_and(FmValue::is_bool_true),
            disable_model_invocation: fm
                .get("disable-model-invocation")
                .is_some_and(FmValue::is_bool_true),
            source,
            managed_project_boundary,
            managed_root: None,
            trusted,
        }),
        diags,
    )
}

/// Ports pi's `validateName` (skills.ts:92-112). Lengths are JS `String.length`
/// — UTF-16 code units — not bytes.
fn validate_name(name: &str) -> Vec<String> {
    let mut errs = Vec::new();
    let n = utf16_len(name);
    if n > MAX_SKILL_NAME_LENGTH {
        errs.push(format!(
            "name exceeds {MAX_SKILL_NAME_LENGTH} characters ({n})"
        ));
    }
    if !is_valid_skill_name(name) {
        errs.push(
            "name contains invalid characters (must be lowercase a-z, 0-9, hyphens only)"
                .to_string(),
        );
    }
    if name.starts_with('-') || name.ends_with('-') {
        errs.push("name must not start or end with a hyphen".to_string());
    }
    if name.contains("--") {
        errs.push("name must not contain consecutive hyphens".to_string());
    }
    errs
}

fn is_valid_skill_name(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    name.chars()
        .all(|r| (r >= 'a' && r <= 'z') || (r >= '0' && r <= '9') || r == '-')
}

/// Ports pi's `validateDescription` (skills.ts:117-127).
fn validate_description(desc: &str) -> Vec<String> {
    let mut errs = Vec::new();
    if desc.trim().is_empty() {
        errs.push("description is required".to_string());
    } else {
        let n = utf16_len(desc);
        if n > MAX_SKILL_DESCRIPTION_LENGTH {
            errs.push(format!(
                "description exceeds {MAX_SKILL_DESCRIPTION_LENGTH} characters ({n})"
            ));
        }
    }
    errs
}

fn to_posix(p: &str) -> String {
    p.replace('\\', "/")
}

// ---------------------------------------------------------------------------
// Frontmatter
// ---------------------------------------------------------------------------
#[derive(Clone, Debug)]
struct FmValue {
    value: String,
    kind: FmKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FmKind {
    Plain,
    Quoted,
    Block,
}

impl FmValue {
    /// Reports whether the value is the YAML boolean `true`: a plain (unquoted)
    /// scalar parsing to `true`/`True`/`TRUE`. Quoted `"true"` is a string.
    fn is_bool_true(&self) -> bool {
        self.kind == FmKind::Plain
            && (self.value == "true" || self.value == "True" || self.value == "TRUE")
    }
}

fn parse_frontmatter_list(value: &FmValue) -> Vec<String> {
    let raw = value.value.trim();
    if raw.is_empty() {
        return Vec::new();
    }
    if raw.starts_with('[')
        && let Ok(values) = serde_json::from_str::<Vec<String>>(raw)
    {
        return values
            .into_iter()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .collect();
    }
    raw.trim_start_matches('[')
        .trim_end_matches(']')
        .lines()
        .flat_map(|line| line.split(','))
        .map(|value| value.trim().trim_start_matches("- ").trim())
        .map(|value| value.trim_matches(['\'', '"']).to_owned())
        .filter(|value| !value.is_empty())
        .collect()
}

/// Extracts a `--- ... ---` YAML header into a flat scalar map and returns the
/// remaining body. A minimal-but-correct subset for the flat key/scalar
/// frontmatter skills use: plain scalars (with ` #` comment stripping),
/// single/double-quoted strings, block scalars (`|`, `>`, with `-/+` chomping),
/// and multi-line plain scalars folded across continuation lines.
fn parse_frontmatter(content: &str) -> (std::collections::HashMap<String, FmValue>, String) {
    let normalized = content.replace("\r\n", "\n").replace('\r', "\n");
    let mut fm: std::collections::HashMap<String, FmValue> = std::collections::HashMap::new();
    if !normalized.starts_with("---") {
        return (fm, normalized);
    }
    let after = &normalized[3..];
    let Some(end_rel) = after.find("\n---") else {
        return (fm, normalized);
    };
    let end = end_rel;
    let yaml_part = &normalized[4..3 + end];
    let body = normalized[3 + end + 4..].trim_start().to_string();

    let lines: Vec<&str> = yaml_part.split('\n').collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            i += 1;
            continue;
        }
        if let Some(first) = line.chars().next() {
            if first == ' ' || first == '\t' {
                // Continuation lines are consumed by their key below.
                i += 1;
                continue;
            }
        }
        let Some(idx) = line.find(':') else {
            i += 1;
            continue;
        };
        let key = line[..idx].trim().to_string();
        let rest = line[idx + 1..].trim_start().to_string();

        // Block scalar: | or > with optional chomping indicator.
        if is_block_indicator(&rest) {
            let (val, next) = parse_block_scalar(&rest, &lines, i + 1);
            fm.insert(
                key,
                FmValue {
                    value: val,
                    kind: FmKind::Block,
                },
            );
            i = next;
            continue;
        }

        // Quoted scalar.
        if let Some(v) = parse_quoted_scalar(&rest) {
            fm.insert(
                key,
                FmValue {
                    value: v,
                    kind: FmKind::Quoted,
                },
            );
            i += 1;
            continue;
        }

        // Plain scalar: strip trailing comment, fold continuation lines.
        let mut val = strip_plain_comment(&rest);
        let mut j = i + 1;
        while j < lines.len() {
            let cont = lines[j];
            if cont.is_empty() {
                break;
            }
            let first = cont.chars().next();
            if first != Some(' ') && first != Some('\t') {
                break;
            }
            let cont_trimmed = cont.trim();
            if cont_trimmed.is_empty() || cont_trimmed.starts_with('#') {
                break;
            }
            if !val.is_empty() {
                if cont_trimmed.starts_with("- ") {
                    val.push('\n');
                } else {
                    val.push(' ');
                }
            }
            val.push_str(&strip_plain_comment(cont_trimmed));
            j += 1;
        }
        i = j;
        fm.insert(
            key,
            FmValue {
                value: val,
                kind: FmKind::Plain,
            },
        );
    }
    (fm, body)
}

/// Reports whether a value is a YAML block scalar header: `|` or `>` optionally
/// followed by a chomping indicator (`-` or `+`).
fn is_block_indicator(s: &str) -> bool {
    let first = match s.chars().next() {
        Some(c) if c == '|' || c == '>' => c,
        _ => return false,
    };
    let rest = &s[1..];
    rest.is_empty() || rest == "-" || rest == "+"
}

/// Consumes the indented block following a `|` / `>` header, returning the scalar
/// value and the index of the first unconsumed line.
fn parse_block_scalar(header: &str, lines: &[&str], start: usize) -> (String, usize) {
    let folded = header.starts_with('>');
    let chomp = if header.len() > 1 {
        header.as_bytes()[1]
    } else {
        0
    }; // 0 = clip, '-' = strip, '+' = keep

    // Collect the block: lines more indented than the key (or blank).
    let mut block: Vec<String> = Vec::new();
    let mut indent: i64 = -1;
    let mut i = start;
    while i < lines.len() {
        let line = lines[i];
        if line.trim().is_empty() {
            block.push(String::new());
            i += 1;
            continue;
        }
        let line_indent = (line.len() - line.trim_start().len()) as i64;
        if line_indent == 0 {
            break;
        }
        if indent == -1 {
            indent = line_indent;
        }
        if line_indent < indent {
            break;
        }
        block.push(line[indent as usize..].to_string());
        i += 1;
    }
    // Drop trailing blank lines from the block (they belong to chomping).
    let mut trailing_blanks = 0;
    while block.last() == Some(&String::new()) {
        block.pop();
        trailing_blanks += 1;
    }

    let val = if folded {
        // Fold: newlines between lines become spaces; blank lines become \n.
        let mut b = String::new();
        let mut prev_blank = true; // suppress leading separator
        for l in &block {
            if l.is_empty() {
                b.push('\n');
                prev_blank = true;
                continue;
            }
            if !prev_blank {
                b.push(' ');
            }
            b.push_str(l);
            prev_blank = false;
        }
        b
    } else {
        block.join("\n")
    };

    let val = match chomp {
        b'-' => val, // strip: no trailing newline
        b'+' => format!("{}{}", val, "\n".repeat(trailing_blanks + 1)),
        // clip: one trailing newline only when the block had content (pi: len(block) > 0).
        _ if !block.is_empty() => format!("{val}\n"),
        _ => val,
    };
    (val, i)
}

fn parse_quoted_scalar(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    if bytes.len() < 2 {
        return None;
    }
    let q = bytes[0];
    if (q != b'"' && q != b'\'') || bytes[bytes.len() - 1] != q {
        return None;
    }
    let inner = &s[1..s.len() - 1];
    if q == b'\'' {
        return Some(inner.replace("''", "'"));
    }
    // Double quotes: minimal escape handling.
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('\\') => out.push('\\'),
                Some('"') => out.push('"'),
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    Some(out)
}

/// Removes a trailing ` #comment` from a plain scalar (YAML treats space-then-#
/// as a comment in plain context).
fn strip_plain_comment(s: &str) -> String {
    if let Some(idx) = s.find(" #") {
        return s[..idx].trim_end_matches([' ', '\t']).to_string();
    }
    s.to_string()
}

// ---------------------------------------------------------------------------
// Ignore matching
// ---------------------------------------------------------------------------

/// Accumulates gitignore-style rules from `.gitignore`/`.ignore`/`.fdignore`
/// files found while descending the skill tree. Patterns are stored already
/// prefixed with their directory's root-relative path.
#[derive(Default)]
struct SkillIgnore {
    rules: Vec<SkillIgnoreRule>,
    seen: Vec<String>,
}

#[derive(Clone)]
struct SkillIgnoreRule {
    pattern: String, // prefixed, slashes normalized, leading "/" stripped
    negated: bool,
    dir_only: bool,
}

impl SkillIgnore {
    fn new() -> Self {
        Self::default()
    }

    fn add_rules_from_directory(
        &mut self,
        directory: &crate::fs::CapabilityDirectory,
        relative: &Path,
    ) {
        let key = to_posix(&ps(relative));
        if self.seen.contains(&key) {
            return;
        }
        self.seen.push(key.clone());
        let prefix = if key.is_empty() || key == "." {
            String::new()
        } else {
            format!("{key}/")
        };
        for name in SKILL_IGNORE_FILE_NAMES {
            let Ok(file) = directory.open_file(Path::new(name)) else {
                continue;
            };
            let Ok(bytes) = file.read_bounded(MAX_RESOURCE_FILE_BYTES) else {
                continue;
            };
            let Ok(data) = String::from_utf8(bytes) else {
                continue;
            };
            for line in data.replace("\r\n", "\n").split('\n') {
                if let Some(rule) = prefix_ignore_pattern(line, &prefix) {
                    self.rules.push(rule);
                }
            }
        }
    }

    /// Reports whether the root-relative posix path is ignored. The last matching
    /// rule wins; a negated match un-ignores.
    fn ignores(&self, rel_posix: &str, is_dir: bool) -> bool {
        let rel_posix = rel_posix.trim_end_matches('/');
        let mut ignored = false;
        for r in &self.rules {
            if r.dir_only && !is_dir {
                continue;
            }
            if gitignore_match_path(&r.pattern, rel_posix) {
                ignored = !r.negated;
            }
        }
        ignored
    }
}

/// Ports `prefixIgnorePattern`: trims comments/blank, handles `!`/`\!` negation
/// and `\#` escapes, strips a leading `/`, and prefixes the pattern with the
/// directory prefix.
fn prefix_ignore_pattern(line: &str, prefix: &str) -> Option<SkillIgnoreRule> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with('#') && !trimmed.starts_with("\\#") {
        return None;
    }

    let mut pattern = line;
    let mut negated = false;
    if let Some(rest) = pattern.strip_prefix('!') {
        negated = true;
        pattern = rest;
    } else if let Some(rest) = pattern.strip_prefix("\\!") {
        pattern = rest;
    }
    if let Some(rest) = pattern.strip_prefix('/') {
        pattern = rest;
    }
    let pattern = pattern.trim();
    if pattern.is_empty() {
        return None;
    }
    let dir_only = pattern.ends_with('/');
    let pattern = pattern.trim_end_matches('/');

    Some(SkillIgnoreRule {
        pattern: format!("{prefix}{pattern}"),
        negated,
        dir_only,
    })
}

/// Reports whether `path` (root-relative posix) matches a gitignore `pattern`.
/// Patterns without a `/` match on any path component (basename); anchored
/// patterns match from the root. A directory pattern also matches descendants.
fn gitignore_match_path(pattern: &str, path: &str) -> bool {
    if pattern.is_empty() {
        return false;
    }
    if !pattern.contains('/') {
        // Unanchored: match the basename of any path segment.
        let base = match path.rfind('/') {
            Some(i) => &path[i + 1..],
            None => path,
        };
        if filepath_match(pattern, base) {
            return true;
        }
        // Also ignore everything beneath a matched directory segment.
        for seg in path.split('/') {
            if filepath_match(pattern, seg) {
                return true;
            }
        }
        return false;
    }
    // Anchored: match the full path, or any ancestor directory of it.
    if filepath_match(pattern, path) {
        return true;
    }
    if path.starts_with(&format!("{pattern}/")) {
        return true;
    }
    false
}

/// Ports Go's `path.Match` (filepath.Match on Unix): a single-pattern glob with
/// `/` separator. `*` matches any sequence of non-`/` characters, `?` a single
/// non-`/` character, `[...]` a char class (with ranges and `^` negation), `\`
/// escapes the next char. Byte-oriented for ASCII (skill/gitignore patterns).
fn filepath_match(pattern: &str, name: &str) -> bool {
    match_path(pattern.as_bytes(), name.as_bytes())
}

fn match_path<'a>(mut pattern: &'a [u8], mut name: &'a [u8]) -> bool {
    'pattern: loop {
        if pattern.is_empty() {
            return name.is_empty();
        }
        let (star, chunk, rest) = scan_chunk(pattern);
        pattern = rest;
        if star && chunk.is_empty() {
            // Trailing `*` matches the rest unless it contains a separator.
            return !name.contains(&b'/');
        }
        // Look for a match at the current position.
        let (t, ok) = match_chunk(chunk, name);
        // If we're the last chunk, make sure we've exhausted the name; otherwise
        // we could still match via a following star.
        if ok && (t.is_empty() || !pattern.is_empty()) {
            name = t;
            continue 'pattern;
        }
        if star {
            // Look for a match skipping i+1 bytes. Cannot skip a separator.
            let mut i = 0;
            while i < name.len() && name[i] != b'/' {
                let (t2, ok2) = match_chunk(chunk, &name[i + 1..]);
                if ok2 && !(pattern.is_empty() && !t2.is_empty()) {
                    name = t2;
                    continue 'pattern;
                }
                i += 1;
            }
        }
        return false;
    }
}

/// Mirrors Go's `scanChunk`: leading stars, then the chunk up to the next
/// non-bracketed `*`.
fn scan_chunk(pattern: &[u8]) -> (bool, &[u8], &[u8]) {
    let mut star = false;
    let mut p = pattern;
    while !p.is_empty() && p[0] == b'*' {
        star = true;
        p = &p[1..];
    }
    let mut in_range = false;
    let mut i = 0;
    while i < p.len() {
        match p[i] {
            b'\\' => {
                if i + 1 < p.len() {
                    i += 1;
                }
            }
            b'[' => in_range = true,
            b']' => in_range = false,
            b'*' if !in_range => return (star, &p[..i], &p[i..]),
            _ => {}
        }
        i += 1;
    }
    (star, p, &[])
}

/// Mirrors Go's `matchChunk`: matches `chunk` at the start of `s`, returning the
/// unconsumed tail of `s` and whether it matched. A malformed pattern (unterminated
/// class, bad escape) yields `false`, matching Go's `ErrBadPattern` → no match.
fn match_chunk<'a>(mut chunk: &'a [u8], mut s: &'a [u8]) -> (&'a [u8], bool) {
    let mut failed = false;
    while !chunk.is_empty() {
        if s.is_empty() {
            failed = true;
        }
        match chunk[0] {
            b'[' => {
                let r = if !failed { s[0] } else { 0 };
                if !failed {
                    s = &s[1..];
                }
                chunk = &chunk[1..];
                let negated = !chunk.is_empty() && chunk[0] == b'^';
                if negated {
                    chunk = &chunk[1..];
                }
                let mut class_match = false;
                let mut nrange = 0;
                loop {
                    if !chunk.is_empty() && chunk[0] == b']' && nrange > 0 {
                        chunk = &chunk[1..];
                        break;
                    }
                    let Some((lo, nchunk)) = get_esc(chunk) else {
                        return (&[], false);
                    };
                    chunk = nchunk;
                    let mut hi = lo;
                    if !chunk.is_empty() && chunk[0] == b'-' {
                        let Some((h, nchunk)) = get_esc(&chunk[1..]) else {
                            return (&[], false);
                        };
                        hi = h;
                        chunk = nchunk;
                    }
                    if !failed && lo <= r && r <= hi {
                        class_match = true;
                    }
                    nrange += 1;
                }
                failed = failed || class_match == negated;
            }
            b'?' => {
                if !failed {
                    failed = s[0] == b'/';
                    s = &s[1..];
                }
                chunk = &chunk[1..];
            }
            b'\\' => {
                chunk = &chunk[1..];
                if chunk.is_empty() {
                    return (&[], false); // bad escape
                }
                if !failed {
                    failed = chunk[0] != s[0];
                    s = &s[1..];
                }
                chunk = &chunk[1..];
            }
            c => {
                if !failed {
                    failed = c != s[0];
                    s = &s[1..];
                }
                chunk = &chunk[1..];
            }
        }
    }
    if failed {
        return (&[], false);
    }
    (s, true)
}

/// Mirrors Go's `getEsc`: a possibly-escaped character from a class. Returns
/// `None` on a malformed pattern (empty, or leading `-`/`]`).
fn get_esc(chunk: &[u8]) -> Option<(u8, &[u8])> {
    if chunk.is_empty() || chunk[0] == b'-' || chunk[0] == b']' {
        return None;
    }
    let mut c = chunk;
    if c[0] == b'\\' {
        c = &c[1..];
        if c.is_empty() {
            return None;
        }
    }
    Some((c[0], &c[1..]))
}

pub fn format_skills_for_prompt(skills: &[Skill]) -> String {
    let visible: Vec<&Skill> = skills
        .iter()
        .filter(|skill| !skill.disable_model_invocation && !skill.hidden && skill.trusted)
        .collect();
    if visible.is_empty() {
        return String::new();
    }
    let mut lines: Vec<String> = vec![
        "\n\nThe following skills provide specialized instructions for specific tasks.".to_string(),
        "Use the read tool with skill://<name> to load a skill when the task matches its description. Skill choice remains model prompt policy; deterministic recommendations may transparently augment this list.".to_string(),
        "When a skill file references a relative path, read it as skill://<name>/<relative-path>; the resolver confines access to that skill's base directory.".to_string(),
        String::new(),
        "<available_skills>".to_string(),
    ];
    for s in &visible {
        lines.push("  <skill>".to_string());
        lines.push(format!("    <name>{}</name>", escape_xml(&s.name)));
        lines.push(format!(
            "    <description>{}</description>",
            escape_xml(&s.description)
        ));
        lines.push(format!(
            "    <location>skill://{}</location>",
            escape_xml(&s.name)
        ));
        lines.push("  </skill>".to_string());
    }
    lines.push("</available_skills>".to_string());
    lines.join("\n")
}

fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Number of UTF-16 code units in `s`, matching JS `String.length` (astral
/// characters count as 2). Shared with compaction.
pub(crate) fn utf16_len(s: &str) -> i64 {
    let mut n = 0i64;
    for r in s.chars() {
        n += if r as u32 > 0xFFFF { 2 } else { 1 };
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn with_home<T>(home: &str, f: impl FnOnce() -> T) -> T {
        // Parallel-safe thread-local override (no env mutation).
        HOME_OVERRIDE.with(|o| o.replace(Some(home.to_string())));
        let res = f();
        HOME_OVERRIDE.with(|o| o.take());
        res
    }

    fn write_file(p: &Path, contents: &str) {
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(p, contents).unwrap();
    }

    fn write_skill(dir: &Path, content: &str) {
        write_file(&dir.join("SKILL.md"), content);
    }

    #[test]
    fn agent_dir_base_prefers_compat_agent_dir_over_rpi_home() {
        use std::ffi::OsStr;

        let home = tempfile_dir();
        let compatibility = home.join("compat-agent");
        let rpi_home = home.join("rpi-home");
        assert_eq!(
            resolve_agent_dir_base(
                Some(compatibility.as_os_str()),
                Some(rpi_home.as_os_str()),
                Some(&home),
            ),
            compatibility,
        );
        assert_eq!(
            resolve_agent_dir_base(
                Some(OsStr::new("")),
                Some(rpi_home.as_os_str()),
                Some(&home),
            ),
            rpi_home,
            "empty PI_CODING_AGENT_DIR must fall through to RPI_HOME",
        );
        assert_eq!(
            resolve_agent_dir_base(Some(OsStr::new("")), Some(OsStr::new("")), Some(&home)),
            home.join(CONFIG_DIR_NAME),
            "empty compatibility variables must fall through to HOME/.rpi",
        );
    }

    #[test]
    fn agent_dir_base_normalizes_selected_compatibility_root() {
        use std::ffi::OsStr;

        let home = tempfile_dir();
        assert_eq!(
            resolve_agent_dir_base(Some(OsStr::new("~")), None, Some(&home)),
            home,
        );
        assert_eq!(
            resolve_agent_dir_base(Some(OsStr::new("~/relocated")), None, Some(&home)),
            home.join("relocated"),
        );
        assert_eq!(
            resolve_agent_dir_base(Some(OsStr::new("relocated")), None, Some(&home)),
            std::env::current_dir()
                .expect("current directory")
                .join("relocated"),
        );
    }

    #[test]
    fn load_project_context_files_ancestor_order() {
        let fixture = tempfile_dir();
        let repository = fixture.join("repository");
        let agent_dir = fixture.join("agent-home");
        let sub = repository.join("a").join("b");
        fs::create_dir_all(repository.join(".git")).unwrap();
        write_file(&repository.join(".git/HEAD"), "ref: refs/heads/main\n");
        fs::create_dir_all(&agent_dir).unwrap();
        fs::create_dir_all(&sub).unwrap();
        write_file(&repository.join("AGENTS.md"), "root rules");
        write_file(&sub.join("CLAUDE.md"), "leaf rules");

        let files = load_context_files_from_agent_dir(&sub, &agent_dir, true).unwrap();
        let contents: Vec<String> = files.iter().map(|f| f.content.clone()).collect();
        let joined = contents.join("|");
        assert!(joined.contains("root rules") && joined.contains("leaf rules"));
        assert!(
            joined.find("root rules").unwrap() < joined.find("leaf rules").unwrap(),
            "expected root before leaf: {joined}"
        );
    }

    /// The retained-lease context loader must serve subdirectory session cwds
    /// too: the ancestor chain from the Git boundary down to the session
    /// directory is walked boundary-first (repo-root AGENTS.md before the
    /// leaf's CLAUDE.md), the walk stops at the boundary (context above it is
    /// never probed), and every level is read through a no-follow capability.
    #[test]
    fn lease_context_loader_walks_boundary_ancestors_for_subdirectory_cwd() {
        let fixture = tempfile_dir();
        let repository = fixture.join("repository");
        let agent_dir = fixture.join("agent-home");
        let sub = repository.join("a").join("b");
        fs::create_dir_all(repository.join(".git")).unwrap();
        write_file(&repository.join(".git/HEAD"), "ref: refs/heads/main\n");
        fs::create_dir_all(&agent_dir).unwrap();
        fs::create_dir_all(&sub).unwrap();
        // Above the boundary: must never be probed once the walk stops.
        write_file(&fixture.join("AGENTS.md"), "above-boundary rules");
        write_file(&repository.join("AGENTS.md"), "root rules");
        write_file(&sub.join("CLAUDE.md"), "leaf rules");

        let capabilities = CapabilityFs::new().unwrap();
        let agent_root = capabilities
            .open_root(
                CapabilityRootPurpose::AgentStore,
                AbsolutePath::parse(agent_dir.clone()).unwrap(),
                CapabilityAccess::ReadOnly,
                RootCreation::Existing,
            )
            .unwrap();
        let project_root = capabilities
            .open_root(
                CapabilityRootPurpose::ProjectResources,
                AbsolutePath::parse(sub.clone()).unwrap(),
                CapabilityAccess::ReadOnly,
                RootCreation::Existing,
            )
            .unwrap();
        let files = load_context_files_from_agent_root_lease(
            &capabilities,
            &agent_root,
            Some(&project_root),
            &sub,
            true,
        )
        .unwrap();

        let joined: String = files
            .iter()
            .map(|file| format!("{}:{}", file.path, file.content))
            .collect::<Vec<_>>()
            .join("|");
        assert!(
            !joined.contains("above-boundary rules"),
            "context above the boundary must never load: {joined}"
        );
        let root_index = joined.find("root rules").expect("boundary context loaded");
        let leaf_index = joined.find("leaf rules").expect("session context loaded");
        assert!(
            root_index < leaf_index,
            "expected boundary before leaf: {joined}"
        );
        assert!(
            files.iter().any(|file| file.path == "AGENTS.md"),
            "paths: {joined}"
        );
        assert!(
            files.iter().any(|file| file.path == "a/b/CLAUDE.md"),
            "paths: {joined}"
        );
    }

    /// When the agent root and the project directory are the same path, the
    /// global load already recorded AGENTS.md. The project level must treat
    /// that seen file as the winning candidate and must not also load CLAUDE.md.
    #[test]
    fn lease_context_loader_skips_later_candidate_when_first_was_already_seen() {
        let fixture = tempfile_dir();
        let repository = fixture.join("repository");
        fs::create_dir_all(repository.join(".git")).unwrap();
        write_file(&repository.join(".git/HEAD"), "ref: refs/heads/main\n");
        write_file(&repository.join("AGENTS.md"), "agents rules");
        write_file(&repository.join("CLAUDE.md"), "claude rules");

        let capabilities = CapabilityFs::new().unwrap();
        let root = capabilities
            .open_root(
                CapabilityRootPurpose::AgentStore,
                AbsolutePath::parse(repository.clone()).unwrap(),
                CapabilityAccess::ReadOnly,
                RootCreation::Existing,
            )
            .unwrap();
        let project = capabilities
            .open_root(
                CapabilityRootPurpose::ProjectResources,
                AbsolutePath::parse(repository.clone()).unwrap(),
                CapabilityAccess::ReadOnly,
                RootCreation::Existing,
            )
            .unwrap();
        let files = load_context_files_from_agent_root_lease(
            &capabilities,
            &root,
            Some(&project),
            &repository,
            true,
        )
        .unwrap();
        let joined: String = files
            .iter()
            .map(|file| file.content.as_str())
            .collect::<Vec<_>>()
            .join("|");
        assert!(
            joined.contains("agents rules"),
            "the first candidate must load: {joined}"
        );
        assert!(
            !joined.contains("claude rules"),
            "a later candidate must not load after a seen winner: {joined}"
        );
    }

    /// A project context candidate that is a symbolic link must fail closed
    /// in the retained-lease loader: the strict no-follow open rejects it and
    /// the load errors instead of reading the link target.
    #[cfg(unix)]
    #[test]
    fn lease_context_loader_symlinked_candidate_fails_closed() {
        use std::os::unix::fs::symlink;

        let fixture = tempfile_dir();
        let repository = fixture.join("repository");
        let agent_dir = fixture.join("agent-home");
        fs::create_dir_all(repository.join(".git")).unwrap();
        write_file(&repository.join(".git/HEAD"), "ref: refs/heads/main\n");
        fs::create_dir_all(&agent_dir).unwrap();
        write_file(&repository.join("real-rules.md"), "linked rules");
        symlink(repository.join("real-rules.md"), repository.join("AGENTS.md")).unwrap();

        let capabilities = CapabilityFs::new().unwrap();
        let agent_root = capabilities
            .open_root(
                CapabilityRootPurpose::AgentStore,
                AbsolutePath::parse(agent_dir.clone()).unwrap(),
                CapabilityAccess::ReadOnly,
                RootCreation::Existing,
            )
            .unwrap();
        let project_root = capabilities
            .open_root(
                CapabilityRootPurpose::ProjectResources,
                AbsolutePath::parse(repository.clone()).unwrap(),
                CapabilityAccess::ReadOnly,
                RootCreation::Existing,
            )
            .unwrap();
        let error = load_context_files_from_agent_root_lease(
            &capabilities,
            &agent_root,
            Some(&project_root),
            &repository,
            true,
        )
        .expect_err("symlinked context candidate must fail closed");
        let message = format!("{error:#}");
        assert!(
            message.contains("symbolic link") || message.contains("Symlink") || message.contains("symlink"),
            "expected symlink rejection, got: {message}"
        );
        assert!(
            !message.contains("linked rules"),
            "link target content must never leak: {message}"
        );
    }

    #[test]
    fn gitdir_regular_file_parses_gitdir_pointer() {
        let fixture = tempfile_dir();
        let repository = fixture.join("repository");
        let git_store = fixture.join("git-store");
        fs::create_dir_all(&repository).unwrap();
        fs::create_dir_all(&git_store).unwrap();
        write_file(&git_store.join("HEAD"), "ref: refs/heads/main\n");
        // A regular-file `.git` (Git worktree control file) opens as a
        // directory with ENOTDIR; the boundary must parse the `gitdir:`
        // pointer instead of rejecting the repository.
        write_file(
            &repository.join(".git"),
            &format!("gitdir: {}\n", git_store.display()),
        );

        let boundary = project_context_boundary(&repository).unwrap();
        assert_eq!(boundary, repository);
    }

    #[test]
    fn gitdir_regular_file_parses_relative_pointer() {
        let fixture = tempfile_dir();
        let repository = fixture.join("repository");
        let git_store = fixture.join("git-store");
        fs::create_dir_all(&repository).unwrap();
        fs::create_dir_all(&git_store).unwrap();
        write_file(&git_store.join("HEAD"), "ref: refs/heads/main\n");
        write_file(&repository.join(".git"), "gitdir: ../git-store\n");

        let boundary = project_context_boundary(&repository).unwrap();
        assert_eq!(boundary, repository);
    }

    #[test]
    fn gitdir_regular_file_without_head_rejects() {
        let fixture = tempfile_dir();
        let repository = fixture.join("repository");
        fs::create_dir_all(&repository).unwrap();
        // A `.git` control file whose pointed-to directory has no HEAD is not
        // a usable Git boundary and must fail closed.
        write_file(&repository.join(".git"), "gitdir: ../empty-store\n");

        let error = project_context_boundary(&repository).unwrap_err();
        assert!(format!("{error:#}").contains("HEAD"), "{error:#}");
    }

    #[cfg(unix)]
    #[test]
    fn git_boundary_symlink_is_rejected() {
        use std::os::unix::fs::symlink;

        let fixture = tempfile_dir();
        let repository = fixture.join("repository");
        let git_store = fixture.join("git-store");
        fs::create_dir_all(&repository).unwrap();
        fs::create_dir_all(&git_store).unwrap();
        write_file(&git_store.join("HEAD"), "ref: refs/heads/main\n");
        symlink(&git_store, repository.join(".git")).unwrap();

        let error = project_context_boundary(&repository).unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("Git boundary") || message.contains("git boundary"),
            "symlinked .git must be rejected, got: {message}"
        );
    }

    #[test]
    fn load_skills_and_format() {
        let home = tempfile_dir();
        let cwd = tempfile_dir();
        let skill_dir = cwd.join(".rpi").join("skills").join("my-skill");
        write_skill(
            &skill_dir,
            "---\nname: my-skill\ndescription: Does a specialized thing for tests\n---\n# body\n",
        );
        let hidden_dir = cwd.join(".rpi").join("skills").join("hidden");
        write_skill(
            &hidden_dir,
            "---\nname: hidden\ndescription: Should not appear\ndisable-model-invocation: true\n---\n",
        );

        with_home(&home.to_string_lossy(), || {
            let skills = load_skills(&cwd.to_string_lossy());
            assert_eq!(skills.len(), 2, "expected 2 skills, got {:?}", skills);
            let prompt = format_skills_for_prompt(&skills);
            assert!(
                prompt.contains("<name>my-skill</name>"),
                "skill missing: {prompt}"
            );
            assert!(
                !prompt.contains("hidden"),
                "disabled skill excluded: {prompt}"
            );
            assert!(
                prompt.contains("<available_skills>"),
                "missing block: {prompt}"
            );
        });
    }

    #[test]
    fn load_skills_preserves_discovery_order() {
        let home = tempfile_dir();
        let cwd = tempfile_dir();
        write_skill(
            &home.join(".rpi").join("skills").join("z-user-skill"),
            "---\nname: z-user-skill\ndescription: user skill\n---\n",
        );
        write_skill(
            &cwd.join(".rpi").join("skills").join("a-project-skill"),
            "---\nname: a-project-skill\ndescription: project skill\n---\n",
        );
        with_home(&home.to_string_lossy(), || {
            let skills = load_skills(&cwd.to_string_lossy());
            assert_eq!(skills.len(), 2, "{:?}", skills);
            assert_eq!(skills[0].name, "z-user-skill");
            assert_eq!(skills[1].name, "a-project-skill");
        });
    }

    #[test]
    fn skill_block_scalar_description() {
        let home = tempfile_dir();
        let cwd = tempfile_dir();
        write_skill(
            &cwd.join(".rpi").join("skills").join("folded"),
            "---\nname: folded\ndescription: >-\n  Line one of the description\n  continues on line two.\n---\nbody\n",
        );
        with_home(&home.to_string_lossy(), || {
            let skills = load_skills(&cwd.to_string_lossy());
            assert_eq!(skills.len(), 1, "{:?}", skills);
            assert_eq!(
                skills[0].description,
                "Line one of the description continues on line two."
            );
        });
    }

    #[test]
    fn parse_frontmatter_block_scalars() {
        let (fm, _) = parse_frontmatter(
            "---\nlit: |\n  a\n  b\nstrip: |-\n  a\n  b\nfold: >\n  a\n  b\n---\nbody",
        );
        assert_eq!(fm["lit"].value, "a\nb\n", "literal clip");
        assert_eq!(fm["strip"].value, "a\nb", "literal strip");
        assert_eq!(fm["fold"].value, "a b\n", "folded clip");
    }

    #[test]
    fn parse_frontmatter_multiline_plain() {
        let (fm, _) =
            parse_frontmatter("---\ndescription: starts here\n  and continues here\n---\n");
        assert_eq!(fm["description"].value, "starts here and continues here");
    }

    #[test]
    fn skill_disable_model_invocation_strict_bool() {
        let home = tempfile_dir();
        let cwd = tempfile_dir();
        write_skill(
            &cwd.join(".rpi").join("skills").join("plain-true"),
            "---\nname: plain-true\ndescription: d\ndisable-model-invocation: true\n---\n",
        );
        write_skill(
            &cwd.join(".rpi").join("skills").join("quoted-true"),
            "---\nname: quoted-true\ndescription: d\ndisable-model-invocation: \"true\"\n---\n",
        );
        write_skill(
            &cwd.join(".rpi").join("skills").join("yaml-caps-true"),
            "---\nname: yaml-caps-true\ndescription: d\ndisable-model-invocation: True\n---\n",
        );
        with_home(&home.to_string_lossy(), || {
            let mut by_name = std::collections::HashMap::new();
            for s in load_skills(&cwd.to_string_lossy()) {
                by_name.insert(s.name.clone(), s);
            }
            assert_eq!(by_name.len(), 3);
            assert!(by_name["plain-true"].disable_model_invocation, "plain true");
            assert!(
                !by_name["quoted-true"].disable_model_invocation,
                "quoted true is a string"
            );
            assert!(
                by_name["yaml-caps-true"].disable_model_invocation,
                "YAML True"
            );
        });
    }

    #[test]
    fn skill_validation_lengths_utf16() {
        // 513 astral chars = 513 runes but 1026 UTF-16 units > 1024.
        let desc = "\u{1F600}".repeat(513);
        let errs = validate_description(&desc);
        assert_eq!(errs.len(), 1);
        assert!(errs[0].contains("description exceeds 1024 characters (1026)"));
        // 512 astral chars = 1024 units, exactly at the limit, valid.
        let errs = validate_description(&"\u{1F600}".repeat(512));
        assert!(
            errs.is_empty(),
            "1024 UTF-16 units should be valid: {errs:?}"
        );
    }

    #[test]
    fn load_skills_root_markdown_child() {
        let home = tempfile_dir();
        let cwd = tempfile_dir();
        let skills_root = cwd.join(".rpi").join("skills");
        fs::create_dir_all(&skills_root).unwrap();
        write_file(
            &skills_root.join("top.md"),
            "---\nname: top\ndescription: top-level skill\n---\n# body\n",
        );
        write_skill(
            &skills_root.join("sub"),
            "---\nname: sub\ndescription: subdir skill\n---\n",
        );
        with_home(&home.to_string_lossy(), || {
            let skills = load_skills(&cwd.to_string_lossy());
            let names: Vec<String> = skills.iter().map(|s| s.name.clone()).collect();
            assert!(names.contains(&"top".to_string()), "{names:?}");
            assert!(names.contains(&"sub".to_string()), "{names:?}");
        });
    }

    #[test]
    fn load_skills_skill_root_stops_recursion() {
        let home = tempfile_dir();
        let cwd = tempfile_dir();
        let root = cwd.join(".rpi").join("skills").join("rooted");
        write_skill(&root, "---\nname: rooted\ndescription: root skill\n---\n");
        write_file(
            &root.join("extra.md"),
            "---\nname: extra\ndescription: should not load\n---\n",
        );
        write_skill(
            &root.join("nested"),
            "---\nname: nested\ndescription: should not load\n---\n",
        );
        with_home(&home.to_string_lossy(), || {
            let skills = load_skills(&cwd.to_string_lossy());
            assert_eq!(skills.len(), 1, "{:?}", skills);
            assert_eq!(skills[0].name, "rooted");
        });
    }

    #[test]
    fn skill_selector_frontmatter_metadata() {
        let home = tempfile_dir();
        let cwd = tempfile_dir();
        write_skill(
            &cwd.join(".rpi").join("skills").join("metadata"),
            "---\nname: metadata\ndescription: selector metadata\nglobs: [\"**/*.rs\", \"Cargo.toml\"]\nalwaysApply: true\nhide: true\n---\nbody\n",
        );
        with_home(&home.to_string_lossy(), || {
            let skills = load_skills(&cwd.to_string_lossy());
            assert_eq!(skills.len(), 1);
            assert_eq!(skills[0].globs, vec!["**/*.rs", "Cargo.toml"]);
            assert!(skills[0].always_apply);
            assert!(skills[0].hidden);
            assert_eq!(skills[0].source, SkillSource::Project);
            assert!(skills[0].trusted);
            assert!(format_skills_for_prompt(&skills).is_empty());
        });
    }

    #[test]
    fn untrusted_project_skill_is_never_discovered() {
        let home = tempfile_dir();
        let cwd = tempfile_dir();
        write_skill(
            &cwd.join(".rpi").join("skills").join("project-only"),
            "---\nname: project-only\ndescription: project skill\n---\nbody\n",
        );
        with_home(&home.to_string_lossy(), || {
            let (skills, diagnostics) = load_skills_trusted(&cwd.to_string_lossy(), false);
            assert!(skills.is_empty());
            assert!(diagnostics.is_empty());
        });
    }

    #[test]
    fn load_skills_honors_ignore_files() {
        let home = tempfile_dir();
        let cwd = tempfile_dir();
        let skills_root = cwd.join(".rpi").join("skills");
        write_skill(
            &skills_root.join("kept"),
            "---\nname: kept\ndescription: kept skill\n---\n",
        );
        write_skill(
            &skills_root.join("ignored"),
            "---\nname: ignored\ndescription: ignored\n---\n",
        );
        write_file(&skills_root.join(".gitignore"), "ignored/\n");
        with_home(&home.to_string_lossy(), || {
            let skills = load_skills(&cwd.to_string_lossy());
            let names: Vec<String> = skills.iter().map(|s| s.name.clone()).collect();
            assert!(names.contains(&"kept".to_string()), "{names:?}");
            assert!(!names.contains(&"ignored".to_string()), "{names:?}");
        });
    }

    #[test]
    fn load_skills_skips_node_modules() {
        let home = tempfile_dir();
        let cwd = tempfile_dir();
        let skills_root = cwd.join(".rpi").join("skills");
        write_skill(
            &skills_root.join("node_modules").join("nested"),
            "---\nname: nested\ndescription: in node_modules\n---\n",
        );
        write_skill(
            &skills_root.join("visible"),
            "---\nname: visible\ndescription: visible\n---\n",
        );
        with_home(&home.to_string_lossy(), || {
            let skills = load_skills(&cwd.to_string_lossy());
            let names: Vec<String> = skills.iter().map(|s| s.name.clone()).collect();
            assert!(!names.contains(&"nested".to_string()), "{names:?}");
            assert!(names.contains(&"visible".to_string()), "{names:?}");
        });
    }

    #[test]
    fn skill_name_validation_diagnostics() {
        let home = tempfile_dir();
        let cwd = tempfile_dir();
        // Invalid name but present description → loads with warnings.
        write_skill(
            &cwd.join(".rpi").join("skills").join("bad"),
            "---\nname: Bad_Name\ndescription: ok\n---\n",
        );
        // Missing description → dropped entirely.
        write_skill(
            &cwd.join(".rpi").join("skills").join("nodesc"),
            "---\nname: nodesc\n---\n",
        );
        with_home(&home.to_string_lossy(), || {
            let (skills, diags) = load_skills_with_diagnostics(&cwd.to_string_lossy());
            let names: Vec<String> = skills.iter().map(|s| s.name.clone()).collect();
            assert!(names.contains(&"Bad_Name".to_string()), "{names:?}");
            assert!(!names.contains(&"nodesc".to_string()), "{names:?}");
            let msgs: Vec<String> = diags.iter().map(|d| d.message.clone()).collect();
            assert!(
                msgs.iter().any(|m| m.contains("invalid characters")),
                "expected name warning: {msgs:?}"
            );
            assert!(
                msgs.iter().any(|m| m == "description is required"),
                "expected missing-desc diag: {msgs:?}"
            );
        });
    }

    #[test]
    fn validate_name_accepts_well_formed() {
        assert!(validate_name("good-name").is_empty());
        assert!(!validate_name("Bad").is_empty());
        assert!(
            validate_name("").contains(
                &"name contains invalid characters (must be lowercase a-z, 0-9, hyphens only)"
                    .to_string()
            )
        );
    }

    #[test]
    fn filepath_match_basics() {
        assert!(filepath_match("*.md", "AGENTS.md"));
        assert!(filepath_match("AGENTS.md", "AGENTS.md"));
        assert!(!filepath_match("*.md", "sub/file.md"), "* must not cross /");
        assert!(filepath_match("a*", "abc"));
        assert!(filepath_match("a?c", "abc"));
        assert!(filepath_match("[abc]bc", "abc"));
        assert!(filepath_match("[^abc]bc", "dbc"));
        assert!(
            !filepath_match("[!abc]bc", "dbc"),
            "! is a literal class member in path.Match"
        );
        assert!(filepath_match("[a-f]oo", "boo"));
        assert!(filepath_match("a\\*b", "a*b"));
        assert!(!filepath_match("a\\*b", "axxb"));
    }

    #[test]
    fn gitignore_match_basics() {
        assert!(gitignore_match_path("foo", "foo"));
        assert!(gitignore_match_path("foo", "a/foo"));
        assert!(gitignore_match_path("foo", "foo/bar"));
        assert!(gitignore_match_path("a/b", "a/b"));
        assert!(gitignore_match_path("a/b", "a/b/c"));
        assert!(!gitignore_match_path("a/b", "x/a/b"));
        assert!(gitignore_match_path("*.md", "sub/AGENTS.md"));
    }

    fn tempfile_dir() -> PathBuf {
        // std::env::temp_dir + unique suffix
        let base = std::env::temp_dir();
        let unique = format!("pi-coding-test-{}", uuidv7ish());
        let p = base.join(unique);
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn uuidv7ish() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("{nanos:x}")
    }

    /// Adversarial: a global context file (AGENTS.md) symlinked INTO the
    /// project boundary must be rejected so an untrusted project cannot
    /// inject content into the global managed context that loads even when
    /// the project is untrusted.
    #[cfg(unix)]
    #[test]
    fn global_context_symlink_into_project_boundary_is_rejected() {
        use std::os::unix::fs::symlink;

        let fixture = tempfile_dir();
        let repository = fixture.join("repository");
        let agent_dir = fixture.join("agent-home");
        let cwd = repository.join("nested");
        fs::create_dir_all(&repository).unwrap();
        fs::create_dir_all(&repository.join(".git")).unwrap();
        write_file(&repository.join(".git/HEAD"), "ref: refs/heads/main\n");
        fs::create_dir_all(&agent_dir).unwrap();
        fs::create_dir_all(&cwd).unwrap();
        // Project content that must never surface through a global link.
        write_file(&repository.join("evil.md"), "project-injection-payload");
        // Global AGENTS.md symlinked into the project boundary.
        symlink(repository.join("evil.md"), agent_dir.join("AGENTS.md")).unwrap();

        let capabilities = CapabilityFs::new().unwrap();
        let error = load_context_files_from_agent_dir_with_capabilities(
            &capabilities,
            &cwd,
            &agent_dir,
            true,
        )
        .expect_err("global symlink into project boundary must be rejected");
        let message = format!("{error:#}");
        assert!(
            message.contains("project boundary") || message.contains("symbolic link"),
            "expected boundary rejection, got: {message}"
        );
        // The project-injection payload must never surface in the error or
        // any loaded content.
        assert!(
            !message.contains("project-injection-payload"),
            "file contents must not leak into diagnostics: {message}"
        );
    }

    /// Adversarial: a chained managed link (A → B → final target) whose
    /// source lives in the agent dir and whose final target stays outside
    /// the project boundary must resolve and load, exercising the bounded
    /// hop loop and identity re-verification.
    #[cfg(unix)]
    #[test]
    fn global_context_chained_managed_links_resolve_outside_project() {
        use std::os::unix::fs::symlink;

        let fixture = tempfile_dir();
        let repository = fixture.join("repository");
        let agent_dir = fixture.join("agent-home");
        let outside = fixture.join("dotfiles");
        let cwd = repository.join("nested");
        fs::create_dir_all(&repository).unwrap();
        fs::create_dir_all(&repository.join(".git")).unwrap();
        write_file(&repository.join(".git/HEAD"), "ref: refs/heads/main\n");
        fs::create_dir_all(&agent_dir).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::create_dir_all(&cwd).unwrap();
        write_file(&outside.join("AGENTS.md"), "chained-global-rules");
        // Chain: agent_dir/AGENTS.md → agent_dir/link2 → outside/AGENTS.md
        symlink(outside.join("AGENTS.md"), agent_dir.join("link2")).unwrap();
        symlink(agent_dir.join("link2"), agent_dir.join("AGENTS.md")).unwrap();

        let capabilities = CapabilityFs::new().unwrap();
        let files = load_context_files_from_agent_dir_with_capabilities(
            &capabilities,
            &cwd,
            &agent_dir,
            true,
        )
        .expect("chained managed link outside project must load");
        let global = files
            .iter()
            .find(|file| file.content.contains("chained-global-rules"))
            .expect("chained global context must load");
        assert!(global.path.contains("AGENTS.md"), "path: {}", global.path);
    }

    /// Adversarial: a relative symlink with `..` traversal in a global
    /// context path must resolve to a target outside the project boundary
    /// (operator's own dotfiles layout) and load, proving lexical `..`
    /// normalization doesn't break legitimate managed links.
    #[cfg(unix)]
    #[test]
    fn global_context_relative_link_with_parent_traversal_loads() {
        use std::os::unix::fs::symlink;

        let fixture = tempfile_dir();
        let repository = fixture.join("repository");
        let home = fixture.join("home");
        let agent_dir = home.join(".rpi");
        let dotfiles = home.join("dotfiles");
        let cwd = repository.join("nested");
        fs::create_dir_all(&repository).unwrap();
        fs::create_dir_all(&repository.join(".git")).unwrap();
        write_file(&repository.join(".git/HEAD"), "ref: refs/heads/main\n");
        fs::create_dir_all(&agent_dir).unwrap();
        fs::create_dir_all(&dotfiles).unwrap();
        fs::create_dir_all(&cwd).unwrap();
        write_file(&dotfiles.join("AGENTS.md"), "relative-global-rules");
        // Relative link: agent_dir/AGENTS.md → ../dotfiles/AGENTS.md
        // (one `..` to climb from ~/.rpi to the home directory)
        symlink(
            Path::new("../dotfiles/AGENTS.md"),
            agent_dir.join("AGENTS.md"),
        )
        .unwrap();

        let capabilities = CapabilityFs::new().unwrap();
        let files = load_context_files_from_agent_dir_with_capabilities(
            &capabilities,
            &cwd,
            &agent_dir,
            true,
        )
        .expect("relative managed link with .. must load");
        assert!(
            files
                .iter()
                .any(|file| file.content.contains("relative-global-rules")),
            "relative global context must load"
        );
    }

    #[cfg(unix)]
    #[test]
    fn user_top_level_skills_link_loads_and_invalid_targets_fail_closed() {
        use std::os::unix::fs::symlink;

        let fixture = tempfile_dir();
        let agent_dir = fixture.join("agent");
        let external = fixture.join("external-skills");
        fs::create_dir_all(external.join("core")).unwrap();
        fs::create_dir_all(&agent_dir).unwrap();
        write_file(
            &external.join("core/SKILL.md"),
            "---\nname: core\ndescription: linked core skill\n---\nbody\n",
        );
        let configured = agent_dir.join("skills");
        symlink(&external, &configured).unwrap();
        let capabilities = CapabilityFs::new().unwrap();
        let (loaded, diagnostics) = load_skills_from_dir_with_capabilities(
            &capabilities,
            &configured.to_string_lossy(),
            SkillSource::User,
            true,
            true,
        );
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert_eq!(
            loaded
                .iter()
                .map(|skill| skill.name.as_str())
                .collect::<Vec<_>>(),
            ["core"]
        );

        fs::remove_file(&configured).unwrap();
        symlink(fixture.join("missing-skills"), &configured).unwrap();
        let (_, missing) = load_skills_from_dir_with_capabilities(
            &capabilities,
            &configured.to_string_lossy(),
            SkillSource::User,
            true,
            true,
        );
        assert!(!missing.is_empty(), "missing target must fail closed");

        fs::remove_file(&configured).unwrap();
        let file = fixture.join("not-a-directory");
        write_file(&file, "x");
        symlink(&file, &configured).unwrap();
        let (_, invalid) = load_skills_from_dir_with_capabilities(
            &capabilities,
            &configured.to_string_lossy(),
            SkillSource::User,
            true,
            true,
        );
        assert!(!invalid.is_empty(), "non-directory target must fail closed");
    }

    #[test]
    fn managed_skill_load_stamps_provided_project_boundary() {
        let fixture = tempfile_dir();
        let skills = fixture.join("skills/core");
        fs::create_dir_all(&skills).unwrap();
        write_file(
            &skills.join("SKILL.md"),
            "---\nname: core\ndescription: core skill\n---\nbody\n",
        );
        let boundary = fixture.join("project");
        fs::create_dir_all(&boundary).unwrap();
        let capabilities = CapabilityFs::new().unwrap();
        let (loaded, diagnostics) = load_skills_from_dir_with_capabilities_and_boundary(
            &capabilities,
            &skills.to_string_lossy(),
            SkillSource::User,
            true,
            true,
            Some(boundary.as_path()),
        );
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert_eq!(loaded.len(), 1, "core skill must load");
        assert_eq!(
            loaded[0].managed_project_boundary.as_deref(),
            Some(boundary.as_path()),
            "managed skill must carry the current project exclusion boundary"
        );
    }

    /// A User-source skill whose `SKILL.md` is a managed-config symlink must
    /// record the resolved target path from the same retained open that
    /// produced the file handle — never the unvalidated display path of the
    /// link entry. Guards the single-authoritative-open invariant: no second
    /// managed open, no display-path fallback.
    #[cfg(unix)]
    #[test]
    fn managed_skill_uses_resolved_target_path_not_display_fallback() {
        use std::os::unix::fs::symlink;

        let fixture = tempfile_dir();
        let skills = fixture.join("skills");
        let real = skills.join(".real-core");
        fs::create_dir_all(&real).unwrap();
        write_file(
            &real.join("SKILL.md"),
            "---\nname: core\ndescription: managed core skill\n---\nbody\n",
        );
        let link_dir = skills.join("core");
        fs::create_dir_all(&link_dir).unwrap();
        // The SKILL.md entry itself is a managed link to the real target,
        // both living inside the same managed root.
        symlink(real.join("SKILL.md"), link_dir.join("SKILL.md")).unwrap();

        let capabilities = CapabilityFs::new().unwrap();
        let (loaded, diagnostics) = load_skills_from_dir_with_capabilities_and_boundary(
            &capabilities,
            &skills.to_string_lossy(),
            SkillSource::User,
            true,
            true,
            None,
        );
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert_eq!(
            loaded.len(),
            1,
            "managed core skill must load through symlinked SKILL.md"
        );
        let skill = &loaded[0];
        let resolved = real.join("SKILL.md");
        assert_eq!(
            skill.file_path,
            resolved.to_string_lossy(),
            "file_path must be the resolved target from the single managed open, not the display path"
        );
        assert_eq!(
            skill.base_dir,
            real.to_string_lossy(),
            "base_dir must be the resolved target's parent, not the link directory"
        );
        let display = link_dir.join("SKILL.md");
        assert_ne!(
            skill.file_path,
            display.to_string_lossy(),
            "unvalidated display path of the link entry must not be retained"
        );
    }

    /// A managed skill whose `SKILL.md` symlink target is missing must fail
    /// closed: no skill loads and a diagnostic is surfaced, rather than a
    /// silent fallback to the link's display path. Guards that a missing
    /// resolved path never produces an unvalidated display-path skill.
    #[cfg(unix)]
    #[test]
    fn managed_skill_with_missing_link_target_fails_closed() {
        use std::os::unix::fs::symlink;

        let fixture = tempfile_dir();
        let skills = fixture.join("skills");
        let link_dir = skills.join("core");
        fs::create_dir_all(&link_dir).unwrap();
        // SKILL.md links to a target that does not exist.
        symlink(skills.join("missing.md"), link_dir.join("SKILL.md")).unwrap();

        let capabilities = CapabilityFs::new().unwrap();
        let (loaded, diagnostics) = load_skills_from_dir_with_capabilities_and_boundary(
            &capabilities,
            &skills.to_string_lossy(),
            SkillSource::User,
            true,
            true,
            None,
        );
        assert!(
            loaded.is_empty(),
            "broken managed link must not load a skill"
        );
        assert!(
            !diagnostics.is_empty(),
            "broken managed link must surface a diagnostic, not a silent display-path fallback"
        );
        assert!(
            diagnostics
                .iter()
                .all(|d| d.kind == "error" || d.kind == "warning"),
            "diagnostics must be surfaced as error/warning, not suppressed: {diagnostics:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn managed_skill_discovery_rejects_project_descendants_but_keeps_sibling() {
        use std::os::unix::fs::symlink;

        let fixture = tempfile_dir();
        let project = fixture.join("project");
        let skills = fixture.join("skills");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(skills.join("keep")).unwrap();
        write_file(
            &skills.join("keep/SKILL.md"),
            "---\nname: keep\ndescription: sibling skill\n---\nbody\n",
        );
        let planted = project.join("planted");
        fs::create_dir_all(&planted).unwrap();
        write_file(
            &planted.join("SKILL.md"),
            "---\nname: planted\ndescription: project planted skill\n---\nsecret\n",
        );
        symlink(&planted, skills.join("planted")).unwrap();

        let capabilities = CapabilityFs::new().unwrap();
        let (loaded, diagnostics) = load_skills_from_dir_with_capabilities_and_boundary(
            &capabilities,
            &skills.to_string_lossy(),
            SkillSource::User,
            true,
            true,
            Some(project.as_path()),
        );
        let names = loaded
            .iter()
            .map(|skill| skill.name.as_str())
            .collect::<Vec<_>>();
        assert!(names.contains(&"keep"), "sibling must load: {names:?}");
        assert!(
            !names.contains(&"planted"),
            "a managed link into the project must not load: {names:?} {diagnostics:?}"
        );
        assert!(
            diagnostics.iter().any(|diagnostic| diagnostic
                .message
                .contains("project boundary")),
            "crossing the project boundary must be a diagnostic: {diagnostics:?}"
        );
    }
}
