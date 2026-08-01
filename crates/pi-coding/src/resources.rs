//! Project context discovery (AGENTS.md/CLAUDE.md ancestry + global `.pi/agent`)
//! and Agent Skill discovery/frontmatter. Port of `coding/resources.go`.

use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};

use anyhow::Context;

use crate::system_prompt::ContextFile;
/// Converts a path to a String losslessly (paths are UTF-8 in practice; mirrors
/// Go's filepath string results). `Path`/`PathBuf` have no `Display`.
fn ps(p: impl AsRef<Path>) -> String {
    p.as_ref().to_string_lossy().into_owned()
}

/// pi's per-project/user config directory name.
pub const CONFIG_DIR_NAME: &str = ".pi";

/// Maximum bytes read from any single configurable text resource.
pub const MAX_RESOURCE_FILE_BYTES: u64 = 1024 * 1024;

/// Maximum combined bytes retained in one validated resource snapshot.
pub const MAX_RESOURCE_SNAPSHOT_BYTES: usize = 8 * 1024 * 1024;

/// Read one UTF-8 resource without allocating beyond the per-file cap.
pub fn read_resource_text(path: &Path, kind: &str) -> anyhow::Result<String> {
    let metadata = fs::metadata(path)
        .map_err(anyhow::Error::from)
        .with_context(|| format!("reading {kind} metadata {}", path.display()))?;
    if metadata.len() > MAX_RESOURCE_FILE_BYTES {
        anyhow::bail!(
            "{kind} {} exceeds {} byte limit ({} bytes)",
            path.display(),
            MAX_RESOURCE_FILE_BYTES,
            metadata.len()
        );
    }
    let mut file = File::open(path)
        .map_err(anyhow::Error::from)
        .with_context(|| format!("opening {kind} {}", path.display()))?;
    let capacity = usize::try_from(metadata.len()).unwrap_or(MAX_RESOURCE_FILE_BYTES as usize);
    let mut bytes = Vec::with_capacity(capacity);
    file.take(MAX_RESOURCE_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(anyhow::Error::from)
        .with_context(|| format!("reading {kind} {}", path.display()))?;
    if bytes.len() > MAX_RESOURCE_FILE_BYTES as usize {
        anyhow::bail!("{kind} {} exceeds {} byte limit", path.display(), MAX_RESOURCE_FILE_BYTES);
    }
    String::from_utf8(bytes)
        .with_context(|| format!("decoding {kind} {} as UTF-8", path.display()))
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

/// Returns the global agent config directory (`~/.pi/agent`).
pub fn agent_dir() -> String {
    if let Some(configured) = std::env::var_os("PI_CODING_AGENT_DIR")
        .filter(|value| !value.is_empty())
    {
        return ps(PathBuf::from(configured));
    }
    match home_dir() {
        Some(home) => ps(Path::new(&home).join(CONFIG_DIR_NAME).join("agent")),
        None => ps(Path::new(CONFIG_DIR_NAME).join("agent")),
    }
}

#[must_use]
pub fn agent_dir_path() -> PathBuf {
    PathBuf::from(agent_dir())
}

/// Best-effort home directory (HOME on Unix, USERPROFILE on Windows), mirroring
/// Go's `os.UserHomeDir`.
fn home_dir() -> Option<String> {
    // Test-only override (parallel-safe, no global env mutation).
    HOME_OVERRIDE.with(|o| o.borrow().clone())
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
        if file_exists(&dir.join("package.json")) {
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

fn load_context_file_from_dir(dir: &Path) -> Option<ContextFile> {
    for name in CONTEXT_FILE_CANDIDATES {
        let path = dir.join(name);
        if !path.is_file() {
            continue;
        }
        let data = read_resource_text(&path, "context file").ok()?;
        return Some(ContextFile { path: ps(&path), content: data });
    }
    None
}

/// Resolves symlinks, falling back to the input when it cannot be resolved.
fn canonicalize_path(p: &str) -> String {
    fs::canonicalize(p)
        .map(|c| ps(&c))
        .unwrap_or_else(|_| p.to_string())
}

#[derive(Default, Clone)]
struct GitPaths {
    repo_dir: String,
    common_git_dir: String,
}

/// Walks up from `cwd` for a `.git` entry, handling a regular repo (`.git` is a
/// directory) and a linked worktree (`.git` is a file holding `gitdir: <path>`
/// whose commondir points back at the main repo's git dir). Returns false when no
/// repo is found, when the located git dir has no HEAD, or when a `.git` entry
/// exists but cannot be read.
fn find_git_paths(cwd: &str) -> Option<GitPaths> {
    let mut dir = PathBuf::from(cwd);
    loop {
        let git_path = dir.join(".git");
        match fs::metadata(&git_path) {
            // pi guards on existsSync, which swallows any error and keeps climbing.
            Err(_) => {}
            Ok(st) => {
                if st.is_file() {
                    let content = match fs::read_to_string(&git_path) {
                        Ok(c) => c,
                        Err(_) => return None,
                    };
                    // A .git file that is not a gitdir pointer falls through to parent walk.
                    if let Some(rest) = content
                        .trim_start()
                        .strip_prefix("gitdir: ")
                    {
                        let git_dir = resolve_from(&ps(&dir), rest.trim());
                        if !file_exists(&Path::new(&git_dir).join("HEAD")) {
                            return None;
                        }
                        let mut common_git_dir = git_dir.clone();
                        if let Ok(data) = fs::read_to_string(Path::new(&git_dir).join("commondir")) {
                            common_git_dir = resolve_from(&git_dir, data.trim());
                        }
                        return Some(GitPaths {
                            repo_dir: ps(&dir),
                            common_git_dir,
                        });
                    }
                } else if st.is_dir() {
                    if !file_exists(&git_path.join("HEAD")) {
                        return None;
                    }
                    return Some(GitPaths {
                        repo_dir: ps(&dir),
                        common_git_dir: ps(&git_path),
                    });
                }
            }
        }
        let parent = match dir.parent() {
            Some(p) => p.to_path_buf(),
            None => return None,
        };
        if parent == dir {
            return None;
        }
        dir = parent;
    }
}

/// Node's `path.resolve(base, p)` for a single segment: an absolute `p` wins,
/// otherwise it is joined onto `base`.
fn resolve_from(base: &str, p: &str) -> String {
    if Path::new(p).is_absolute() {
        return clean_path(Path::new(p));
    }
    clean_path(&Path::new(base).join(p))
}

/// Returns the main repo's context file that a nested linked worktree's own copy
/// shadows (both are the same tracked file, so loading both loads it twice).
fn find_shadowed_context_file(cwd: &str) -> Option<String> {
    let gp = find_git_paths(cwd)?;
    let common_git_dir = canonicalize_path(&gp.common_git_dir);
    let worktree_root = canonicalize_path(&gp.repo_dir);
    let main_repo_root = parent_dir(&common_git_dir);
    // False for an ordinary repo (same dir) and for a sibling worktree whose main
    // repo is not an ancestor.
    if !worktree_root.starts_with(&format!("{main_repo_root}/")) {
        return None;
    }
    // The parent of the common git dir is the main worktree root only when that
    // dir is itself checked out from the same repo.
    if canonicalize_path(&format!("{main_repo_root}/.git")) != common_git_dir {
        return None;
    }
    // Selection goes through loadContextFileFromDir, not a cheaper existence check.
    let cf = load_context_file_from_dir(Path::new(&worktree_root))?;
    Some(ps(Path::new(&main_repo_root).join(basename(&cf.path))))
}

fn parent_dir(p: &str) -> String {
    Path::new(p)
        .parent()
        .map(ps)
        .unwrap_or_else(|| p.to_string())
}

fn basename(p: &str) -> String {
    Path::new(p)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Discovers global context and, only when trusted, project/ancestor context.
pub fn load_context_files(cwd: &str, include_project: bool) -> Vec<ContextFile> {
    let cwd = abs_path(Path::new(cwd));
    let agent_dir = agent_dir();
    let mut files = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    if let Some(global) = load_context_file_from_dir(Path::new(&agent_dir)) {
        seen.push(global.path.clone());
        files.push(global);
    }
    if !include_project {
        return files;
    }
    let (shadowed, has_shadowed) = match find_shadowed_context_file(&cwd) {
        Some(path) => (path, true),
        None => (String::new(), false),
    };
    let mut ancestors = Vec::new();
    let mut current = PathBuf::from(&cwd);
    loop {
        if let Some(context) = load_context_file_from_dir(&current) {
            let skip = (has_shadowed && canonicalize_path(&context.path) == shadowed)
                || seen.contains(&context.path);
            if !skip {
                seen.push(context.path.clone());
                ancestors.insert(0, context);
            }
        }
        let Some(parent) = current.parent().map(Path::to_path_buf) else {
            break;
        };
        if parent == current {
            break;
        }
        current = parent;
    }
    files.append(&mut ancestors);
    files
}

/// Compatibility helper for callers that already established project trust.
pub fn load_project_context_files(cwd: &str) -> Vec<ContextFile> {
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

pub fn load_skills_trusted(
    cwd: &str,
    include_project: bool,
) -> (Vec<Skill>, Vec<SkillDiagnostic>) {
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
        load_skills_from_dir(&ps(agent_dir.join("skills")), SkillSource::User, true);
    add(global, global_diagnostics);
    if include_project {
        let (project, project_diagnostics) = load_skills_from_dir(
            &ps(Path::new(cwd).join(CONFIG_DIR_NAME).join("skills")),
            SkillSource::Project,
            true,
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
}

/// A discovered Agent Skill (`SKILL.md` with frontmatter).
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
    pub trusted: bool,
}

/// A validation warning (or error) with the offending file path.
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
fn load_skills_from_dir(
    dir: &str,
    source: SkillSource,
    trusted: bool,
) -> (Vec<Skill>, Vec<SkillDiagnostic>) {
    load_skills_from_dir_internal(
        dir,
        dir,
        true,
        source,
        trusted,
        &mut SkillIgnore::new(),
        &mut Vec::new(),
    )
}

fn load_skills_from_dir_internal(
    dir: &str,
    root: &str,
    include_root_files: bool,
    source: SkillSource,
    trusted: bool,
    ig: &mut SkillIgnore,
    visited: &mut Vec<String>,
) -> (Vec<Skill>, Vec<SkillDiagnostic>) {
    let mut skills: Vec<Skill> = Vec::new();
    let mut diags: Vec<SkillDiagnostic> = Vec::new();

    if !dir_exists(dir) {
        return (skills, diags);
    }
    // realpath-dedup: skip a directory whose canonical path was already visited.
    if let Ok(real) = fs::canonicalize(dir) {
        let real = ps(&real);
        if visited.contains(&real) {
            return (skills, diags);
        }
        visited.push(real);
    }

    ig.add_rules(dir, root);

    // Go's os.ReadDir returns entries sorted by filename; mirror that ordering.
    let mut entries: Vec<fs::DirEntry> = match fs::read_dir(dir) {
        Ok(rd) => rd.filter_map(|e| e.ok()).collect(),
        Err(_) => return (skills, diags),
    };
    entries.sort_by(|a, b| a.file_name().cmp(&b.file_name()));

    // First pass: a SKILL.md in this dir makes it a skill root (stop recursion).
    for e in &entries {
        if e.file_name() != "SKILL.md" {
            continue;
        }
        let full = Path::new(dir).join(e.file_name());
        let (is_file, ok) = stat_is_file(&full, e);
        if !ok {
            continue;
        }
        let rel = to_posix(&rel_path(root, &ps(&full)));
        if !is_file || ig.ignores(&rel, false) {
            continue;
        }
        let (s, d) = load_skill_from_file(&ps(&full), source, trusted);
        diags.extend(d);
        if let Some(skill) = s {
            skills.push(skill);
        }
        return (skills, diags);
    }

    // Second pass: recurse into subdirs and (at the root) load direct .md files.
    for e in &entries {
        let name = e.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with('.') || name_str == "node_modules" {
            continue;
        }
        let full = Path::new(dir).join(&name);
        let (is_dir, is_file) = stat_is_dir_file(&full, e);

        let rel = to_posix(&rel_path(root, &ps(&full)));
        let ignore_path = if is_dir { format!("{rel}/") } else { rel.clone() };
        if ig.ignores(&ignore_path, is_dir) {
            continue;
        }

        if is_dir {
            let (s, d) = load_skills_from_dir_internal(
                &ps(&full),
                root,
                false,
                source,
                trusted,
                ig,
                visited,
            );
            skills.extend(s);
            diags.extend(d);
            continue;
        }

        if !is_file || !include_root_files || !name_str.ends_with(".md") {
            continue;
        }
        let (s, d) = load_skill_from_file(&ps(&full), source, trusted);
        diags.extend(d);
        if let Some(skill) = s {
            skills.push(skill);
        }
    }

    (skills, diags)
}

/// Parses one skill markdown file. The skill loads even with name/description
/// warnings, except when description is missing entirely.
pub(crate) fn load_skill_from_file(
    file_path: &str,
    source: SkillSource,
    trusted: bool,
) -> (Option<Skill>, Vec<SkillDiagnostic>) {
    let mut diags = Vec::new();
    let data = match read_resource_text(Path::new(file_path), "skill") {
        Ok(data) => data,
        Err(error) => {
            return (
                None,
                vec![SkillDiagnostic {
                    kind: "warning".to_string(),
                    message: error.to_string(),
                    path: file_path.to_string(),
                }],
            );
        }
    };
    let (fm, _) = parse_frontmatter(&data);
    let skill_dir = parent_dir(file_path);

    let desc = fm.get("description").map(|v| v.value.clone()).unwrap_or_default();
    for e in validate_description(&desc) {
        diags.push(SkillDiagnostic {
            kind: "warning".to_string(),
            message: e,
            path: file_path.to_string(),
        });
    }

    let mut name = fm.get("name").map(|v| v.value.clone()).unwrap_or_default();
    if name.is_empty() {
        name = basename(&skill_dir);
    }
    for e in validate_name(&name) {
        diags.push(SkillDiagnostic {
            kind: "warning".to_string(),
            message: e,
            path: file_path.to_string(),
        });
    }

    if desc.trim().is_empty() {
        return (None, diags);
    }
    (
        Some(Skill {
            name,
            description: desc,
            file_path: file_path.to_string(),
            base_dir: skill_dir,
            globs: fm
                .get("globs")
                .map(parse_frontmatter_list)
                .unwrap_or_default(),
            always_apply: fm
                .get("alwaysApply")
                .or_else(|| fm.get("always-apply"))
                .map(FmValue::is_bool_true)
                .unwrap_or(false),
            hidden: fm
                .get("hide")
                .or_else(|| fm.get("hidden"))
                .map(FmValue::is_bool_true)
                .unwrap_or(false),
            // Only the YAML boolean `true` (plain/unquoted) enables it.
            disable_model_invocation: fm
                .get("disable-model-invocation")
                .map(|v| v.is_bool_true())
                .unwrap_or(false),
            source,
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
        errs.push(format!("name exceeds {MAX_SKILL_NAME_LENGTH} characters ({n})"));
    }
    if !is_valid_skill_name(name) {
        errs.push("name contains invalid characters (must be lowercase a-z, 0-9, hyphens only)".to_string());
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

fn dir_exists(p: &str) -> bool {
    fs::metadata(p).map(|m| m.is_dir()).unwrap_or(false)
}

fn file_exists(p: &Path) -> bool {
    fs::metadata(p).map(|m| !m.is_dir()).unwrap_or(false)
}

fn rel_path(root: &str, p: &str) -> String {
    let root_path = Path::new(root);
    let p_path = Path::new(p);
    match p_path.strip_prefix(root_path) {
        Ok(rel) => {
            let s = ps(rel);
            if s.is_empty() {
                ".".to_string()
            } else {
                s
            }
        }
        Err(_) => p.to_string(),
    }
}

fn to_posix(p: &str) -> String {
    p.replace('\\', "/")
}

/// Resolves whether `full` is a regular file, following symlinks.
fn stat_is_file(full: &Path, e: &fs::DirEntry) -> (bool, bool) {
    let ft = match e.file_type() {
        Ok(t) => t,
        Err(_) => return (false, false),
    };
    if ft.is_symlink() {
        match fs::metadata(full) {
            Err(_) => return (false, false),
            Ok(info) => return (info.is_file(), true),
        }
    }
    (ft.is_file(), true)
}

/// Resolves dir/file-ness following symlinks. A broken symlink returns
/// `(false,false)` so the caller skips it.
fn stat_is_dir_file(full: &Path, e: &fs::DirEntry) -> (bool, bool) {
    let ft = match e.file_type() {
        Ok(t) => t,
        Err(_) => return (false, false),
    };
    if ft.is_symlink() {
        match fs::metadata(full) {
            Err(_) => return (false, false),
            Ok(info) => return (info.is_dir(), info.is_file()),
        }
    }
    (ft.is_dir(), ft.is_file())
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
            fm.insert(key, FmValue { value: val, kind: FmKind::Block });
            i = next;
            continue;
        }

        // Quoted scalar.
        if let Some(v) = parse_quoted_scalar(&rest) {
            fm.insert(key, FmValue { value: v, kind: FmKind::Quoted });
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
        fm.insert(key, FmValue { value: val, kind: FmKind::Plain });
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
    let chomp = if header.len() > 1 { header.as_bytes()[1] } else { 0 }; // 0 = clip, '-' = strip, '+' = keep

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

/// Parses a fully single- or double-quoted scalar.
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

    /// Loads the ignore files in `dir` (if not already loaded), prefixing each
    /// pattern with `dir`'s path relative to `root`.
    fn add_rules(&mut self, dir: &str, root: &str) {
        if self.seen.contains(&dir.to_string()) {
            return;
        }
        self.seen.push(dir.to_string());

        let rel = rel_path(root, dir);
        let prefix = if rel != "." && !rel.is_empty() {
            format!("{}/", to_posix(&rel))
        } else {
            String::new()
        };

        for fname in SKILL_IGNORE_FILE_NAMES {
            let p = Path::new(dir).join(fname);
            let Ok(data) = fs::read_to_string(&p) else { continue };
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

/// Renders visible skills as the Agent Skills XML block.
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
        lines.push(format!("    <description>{}</description>", escape_xml(&s.description)));
        lines.push(format!("    <location>skill://{}</location>", escape_xml(&s.name)));
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
    fn load_project_context_files_ancestor_order() {
        let root = tempfile_dir();
        let sub = root.join("a").join("b");
        fs::create_dir_all(&sub).unwrap();
        write_file(&root.join("AGENTS.md"), "root rules");
        write_file(&sub.join("CLAUDE.md"), "leaf rules");

        let files = load_project_context_files(&sub.to_string_lossy());
        let contents: Vec<String> = files.iter().map(|f| f.content.clone()).collect();
        let joined = contents.join("|");
        assert!(joined.contains("root rules") && joined.contains("leaf rules"));
        assert!(
            joined.find("root rules").unwrap() < joined.find("leaf rules").unwrap(),
            "expected root before leaf: {joined}"
        );
    }

    #[test]
    fn load_skills_and_format() {
        let home = tempfile_dir();
        let cwd = tempfile_dir();
        let skill_dir = cwd.join(".pi").join("skills").join("my-skill");
        write_skill(&skill_dir, "---\nname: my-skill\ndescription: Does a specialized thing for tests\n---\n# body\n");
        let hidden_dir = cwd.join(".pi").join("skills").join("hidden");
        write_skill(&hidden_dir, "---\nname: hidden\ndescription: Should not appear\ndisable-model-invocation: true\n---\n");

        with_home(&home.to_string_lossy(), || {
            let skills = load_skills(&cwd.to_string_lossy());
            assert_eq!(skills.len(), 2, "expected 2 skills, got {:?}", skills);
            let prompt = format_skills_for_prompt(&skills);
            assert!(prompt.contains("<name>my-skill</name>"), "skill missing: {prompt}");
            assert!(!prompt.contains("hidden"), "disabled skill excluded: {prompt}");
            assert!(prompt.contains("<available_skills>"), "missing block: {prompt}");
        });
    }

    #[test]
    fn load_skills_preserves_discovery_order() {
        let home = tempfile_dir();
        let cwd = tempfile_dir();
        write_skill(&home.join(".pi").join("agent").join("skills").join("z-user-skill"), "---\nname: z-user-skill\ndescription: user skill\n---\n");
        write_skill(&cwd.join(".pi").join("skills").join("a-project-skill"), "---\nname: a-project-skill\ndescription: project skill\n---\n");
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
        write_skill(&cwd.join(".pi").join("skills").join("folded"), "---\nname: folded\ndescription: >-\n  Line one of the description\n  continues on line two.\n---\nbody\n");
        with_home(&home.to_string_lossy(), || {
            let skills = load_skills(&cwd.to_string_lossy());
            assert_eq!(skills.len(), 1, "{:?}", skills);
            assert_eq!(skills[0].description, "Line one of the description continues on line two.");
        });
    }

    #[test]
    fn parse_frontmatter_block_scalars() {
        let (fm, _) = parse_frontmatter("---\nlit: |\n  a\n  b\nstrip: |-\n  a\n  b\nfold: >\n  a\n  b\n---\nbody");
        assert_eq!(fm["lit"].value, "a\nb\n", "literal clip");
        assert_eq!(fm["strip"].value, "a\nb", "literal strip");
        assert_eq!(fm["fold"].value, "a b\n", "folded clip");
    }

    #[test]
    fn parse_frontmatter_multiline_plain() {
        let (fm, _) = parse_frontmatter("---\ndescription: starts here\n  and continues here\n---\n");
        assert_eq!(fm["description"].value, "starts here and continues here");
    }

    #[test]
    fn skill_disable_model_invocation_strict_bool() {
        let home = tempfile_dir();
        let cwd = tempfile_dir();
        write_skill(&cwd.join(".pi").join("skills").join("plain-true"), "---\nname: plain-true\ndescription: d\ndisable-model-invocation: true\n---\n");
        write_skill(&cwd.join(".pi").join("skills").join("quoted-true"), "---\nname: quoted-true\ndescription: d\ndisable-model-invocation: \"true\"\n---\n");
        write_skill(&cwd.join(".pi").join("skills").join("yaml-caps-true"), "---\nname: yaml-caps-true\ndescription: d\ndisable-model-invocation: True\n---\n");
        with_home(&home.to_string_lossy(), || {
            let mut by_name = std::collections::HashMap::new();
            for s in load_skills(&cwd.to_string_lossy()) {
                by_name.insert(s.name.clone(), s);
            }
            assert_eq!(by_name.len(), 3);
            assert!(by_name["plain-true"].disable_model_invocation, "plain true");
            assert!(!by_name["quoted-true"].disable_model_invocation, "quoted true is a string");
            assert!(by_name["yaml-caps-true"].disable_model_invocation, "YAML True");
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
        assert!(errs.is_empty(), "1024 UTF-16 units should be valid: {errs:?}");
    }

    #[test]
    fn load_skills_root_markdown_child() {
        let home = tempfile_dir();
        let cwd = tempfile_dir();
        let skills_root = cwd.join(".pi").join("skills");
        fs::create_dir_all(&skills_root).unwrap();
        write_file(&skills_root.join("top.md"), "---\nname: top\ndescription: top-level skill\n---\n# body\n");
        write_skill(&skills_root.join("sub"), "---\nname: sub\ndescription: subdir skill\n---\n");
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
        let root = cwd.join(".pi").join("skills").join("rooted");
        write_skill(&root, "---\nname: rooted\ndescription: root skill\n---\n");
        write_file(&root.join("extra.md"), "---\nname: extra\ndescription: should not load\n---\n");
        write_skill(&root.join("nested"), "---\nname: nested\ndescription: should not load\n---\n");
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
            &cwd.join(".pi").join("skills").join("metadata"),
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
            &cwd.join(".pi").join("skills").join("project-only"),
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
        let skills_root = cwd.join(".pi").join("skills");
        write_skill(&skills_root.join("kept"), "---\nname: kept\ndescription: kept skill\n---\n");
        write_skill(&skills_root.join("ignored"), "---\nname: ignored\ndescription: ignored\n---\n");
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
        let skills_root = cwd.join(".pi").join("skills");
        write_skill(&skills_root.join("node_modules").join("nested"), "---\nname: nested\ndescription: in node_modules\n---\n");
        write_skill(&skills_root.join("visible"), "---\nname: visible\ndescription: visible\n---\n");
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
        write_skill(&cwd.join(".pi").join("skills").join("bad"), "---\nname: Bad_Name\ndescription: ok\n---\n");
        // Missing description → dropped entirely.
        write_skill(&cwd.join(".pi").join("skills").join("nodesc"), "---\nname: nodesc\n---\n");
        with_home(&home.to_string_lossy(), || {
            let (skills, diags) = load_skills_with_diagnostics(&cwd.to_string_lossy());
            let names: Vec<String> = skills.iter().map(|s| s.name.clone()).collect();
            assert!(names.contains(&"Bad_Name".to_string()), "{names:?}");
            assert!(!names.contains(&"nodesc".to_string()), "{names:?}");
            let msgs: Vec<String> = diags.iter().map(|d| d.message.clone()).collect();
            assert!(msgs.iter().any(|m| m.contains("invalid characters")), "expected name warning: {msgs:?}");
            assert!(msgs.iter().any(|m| m == "description is required"), "expected missing-desc diag: {msgs:?}");
        });
    }

    #[test]
    fn validate_name_accepts_well_formed() {
        assert!(validate_name("good-name").is_empty());
        assert!(!validate_name("Bad").is_empty());
        assert!(validate_name("").contains(&"name contains invalid characters (must be lowercase a-z, 0-9, hyphens only)".to_string()));
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
        assert!(!filepath_match("[!abc]bc", "dbc"), "! is a literal class member in path.Match");
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
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        format!("{nanos:x}")
    }
}