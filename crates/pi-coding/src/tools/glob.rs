//! Glob primitives and the gitignore engine (port of pi's `coding/glob.go`).
//!
//! Shared by the fd-style find matcher (`match_fd_glob`), the rg-style grep
//! matcher (`match_rg_glob`), and the hierarchical gitignore engine
//! (`IgnoreStack`). The gitignore engine is a faithful pure-Rust port so that
//! find's `requireGit=false` (gitignore applies outside a repo, fd
//! `--no-require-git`) and grep's `requireGit=true` (rg) semantics match pi
//! exactly, including nested-repo boundaries and `.git` always being skipped.

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::ApplicationGeneration;
use crate::process::{
    GovernedProcessContext, GovernedProcessSpec, TrustedToolProbe, run_trusted_tool_probe,
};
/// Expands `{a,b}` alternations (globset semantics). Nested braces are expanded
/// recursively. Patterns without braces are returned as-is.
pub(crate) fn expand_braces(pattern: &str) -> Vec<String> {
    let bytes = pattern.as_bytes();
    let start = match bytes.iter().position(|&b| b == b'{') {
        Some(s) => s,
        None => return vec![pattern.to_string()],
    };
    let mut depth = 0i32;
    let mut end = None;
    let mut i = start;
    while i < bytes.len() {
        match bytes[i] {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    end = Some(i);
                }
            }
            _ => {}
        }
        if end.is_some() {
            break;
        }
        i += 1;
    }
    let end = match end {
        Some(e) => e,
        None => return vec![pattern.to_string()],
    };
    let inner = &pattern[start + 1..end];
    let mut alts: Vec<String> = Vec::new();
    let mut depth = 0i32;
    let mut last = 0;
    let inner_bytes = inner.as_bytes();
    let mut i = 0;
    while i < inner_bytes.len() {
        match inner_bytes[i] {
            b'{' => depth += 1,
            b'}' => depth -= 1,
            b',' if depth == 0 => {
                alts.push(inner[last..i].to_string());
                last = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    alts.push(inner[last..].to_string());
    let prefix = &pattern[..start];
    let suffix = &pattern[end + 1..];
    let mut out = Vec::new();
    for a in alts {
        out.extend(expand_braces(&format!("{prefix}{a}{suffix}")));
    }
    out
}

/// Go `filepath.Match` equivalent: matches `pattern` against `name` with `*`
/// (any run of non-separator chars), `?` (single char), and `[...]` classes
/// (ranges via `-`, `[^...]` negated, `]` literal when first). Backslash escapes
/// the next metacharacter.
fn path_match(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    match_impl(&p, &n)
}

fn match_impl(p: &[char], n: &[char]) -> bool {
    let mut pi = 0usize;
    let mut ni = 0usize;
    let mut star_pi: Option<usize> = None;
    let mut star_ni = 0usize;
    while ni < n.len() {
        if pi < p.len() {
            match p[pi] {
                '*' => {
                    star_pi = Some(pi);
                    star_ni = ni;
                    pi += 1;
                    continue;
                }
                '?' => {
                    pi += 1;
                    ni += 1;
                    continue;
                }
                '[' => {
                    match match_class(&p[pi..], n[ni]) {
                        Some((true, consumed)) => {
                            pi += consumed;
                            ni += 1;
                            continue;
                        }
                        Some((false, _)) => {
                            // class parsed but did not match → backtrack via star.
                            if let Some(sp) = star_pi {
                                pi = sp + 1;
                                star_ni += 1;
                                ni = star_ni;
                                continue;
                            }
                            return false;
                        }
                        None => {
                            // malformed class → treat '[' as a literal char.
                            if p[pi] == n[ni] {
                                pi += 1;
                                ni += 1;
                                continue;
                            }
                            if let Some(sp) = star_pi {
                                pi = sp + 1;
                                star_ni += 1;
                                ni = star_ni;
                                continue;
                            }
                            return false;
                        }
                    }
                }
                c if c == n[ni] => {
                    pi += 1;
                    ni += 1;
                    continue;
                }
                _ => {}
            }
        }
        if let Some(sp) = star_pi {
            pi = sp + 1;
            star_ni += 1;
            ni = star_ni;
            continue;
        }
        return false;
    }
    // consume trailing stars
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Matches a character class `[...]`. Returns `(matched, bytes_consumed)` or
/// `None` if the class is malformed (treat `[` as literal). Supports `^`
/// negation, ranges `a-z`, and `]` as a literal when it appears first.
fn match_class(p: &[char], c: char) -> Option<(bool, usize)> {
    // p[0] == '['
    debug_assert_eq!(p[0], '[');
    let mut i = 1usize;
    let mut negated = false;
    if i < p.len() && p[i] == '^' {
        negated = true;
        i += 1;
    }
    let mut matched = false;
    let mut first = true;
    while i < p.len() {
        let ch = p[i];
        if ch == ']' && !first {
            return Some((matched ^ negated, i + 1));
        }
        // range?
        if i + 2 < p.len() && p[i + 1] == '-' && p[i + 2] != ']' {
            let lo = ch;
            let hi = p[i + 2];
            if c >= lo && c <= hi {
                matched = true;
            }
            i += 3;
        } else {
            if c == ch {
                matched = true;
            }
            i += 1;
        }
        first = false;
    }
    None // unterminated class → literal '['
}

/// Matches a single path segment against a glob segment. `[^x]` negated
/// classes are translated from `[!x]`; `fold` lowercases both sides (smart-case).
fn seg_match(pat: &str, seg: &str, fold: bool) -> bool {
    let pat = pat.replace("[!", "[^");
    if fold {
        path_match(&pat.to_lowercase(), &seg.to_lowercase())
    } else {
        path_match(&pat, seg)
    }
}

/// Matches a `/`-segmented glob (supporting `**` crossing slashes and `{a,b}`
/// alternation) against a slash path.
pub(crate) fn glob_match_path(pattern: &str, name: &str, fold: bool) -> bool {
    for p in expand_braces(pattern) {
        if match_glob_one(&p, name, fold) {
            return true;
        }
    }
    false
}

fn match_glob_one(pattern: &str, name: &str, fold: bool) -> bool {
    if pattern == "**" {
        return true;
    }
    match_parts(
        &pattern.split('/').collect::<Vec<_>>(),
        &name.split('/').collect::<Vec<_>>(),
        fold,
    )
}

fn match_parts(pattern: &[&str], name: &[&str], fold: bool) -> bool {
    let mut pattern = pattern.to_vec();
    let mut name = name.to_vec();
    while !pattern.is_empty() {
        if pattern[0] == "**" {
            if pattern.len() == 1 {
                // A trailing "/**" requires at least one more component
                // ("a/**" matches "a/b" but not "a" itself, like git/globset).
                return !name.is_empty();
            }
            // "**" matches zero or more path segments.
            for i in 0..=name.len() {
                if match_parts(&pattern[1..], &name[i..], fold) {
                    return true;
                }
            }
            return false;
        }
        if name.is_empty() {
            return false;
        }
        if !seg_match(pattern[0], name[0], fold) {
            return false;
        }
        pattern.remove(0);
        name.remove(0);
    }
    name.is_empty()
}

/// Reports whether the pattern contains an uppercase letter (fd smart-case:
/// all-lowercase patterns match case-insensitively).
fn pattern_has_upper(pattern: &str) -> bool {
    pattern.chars().any(|c| c.is_uppercase())
}

/// Reports whether a candidate matches a glob pattern using fd `--glob`
/// semantics (find.ts:238-246):
/// - a pattern without `/` matches the basename;
/// - a pattern with `/` matches the absolute candidate path, and fd prepends
///   `**/` unless the pattern starts with `/`, `**/`, or is exactly `**`;
/// - smart-case: an all-lowercase pattern matches case-insensitively.
pub(crate) fn match_fd_glob(pattern: &str, rel: &str, abs: &str) -> bool {
    let pattern = pattern.replace('\\', "/");
    let fold = !pattern_has_upper(&pattern);
    if !pattern.contains('/') {
        return glob_match_path(
            &pattern,
            Path::new(rel)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(""),
            fold,
        );
    }
    let effective = if !pattern.starts_with('/') && !pattern.starts_with("**/") && pattern != "**" {
        format!("**/{pattern}")
    } else {
        pattern
    };
    glob_match_path(&effective, abs, fold)
}

/// Reports whether a root-relative path matches a glob using ripgrep `-g`
/// semantics: a pattern without `/` matches the basename; a pattern containing
/// `/` is anchored to the search root (rg does NOT prepend `**/`). rg `-g`
/// globs are case-sensitive.
pub(crate) fn match_rg_glob(pattern: &str, rel: &str) -> bool {
    let pattern = pattern.replace('\\', "/");
    let rel = rel.replace('\\', "/");
    if !pattern.contains('/') {
        return glob_match_path(
            &pattern,
            Path::new(&rel)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(""),
            false,
        );
    }
    let trimmed = pattern.strip_prefix('/').unwrap_or(&pattern).to_string();
    glob_match_path(&trimmed, &rel, false)
}

// ---------------------------------------------------------------------------
// gitignore engine
// ---------------------------------------------------------------------------

/// A single parsed `.gitignore` rule.
#[derive(Debug, Clone)]
struct IgnorePattern {
    /// Pattern with slashes normalized and a leading `/` stripped.
    pattern: String,
    /// Pattern contained a non-trailing `/` → anchored to its base dir.
    anchored: bool,
    /// Pattern ended with `/` → directories only.
    dir_only: bool,
    negated: bool,
}

/// A pattern list anchored at an absolute base directory (global excludes file,
/// `.git/info/exclude`, or an ancestor `.gitignore`).
#[derive(Debug, Clone)]
struct IgnoreSource {
    base_abs: String,
    pats: Vec<IgnorePattern>,
    /// Sources that apply across nested-repo boundaries (the global
    /// `core.excludesFile`, which git honors in every repo).
    boundary_exempt: bool,
}

/// Applies hierarchical gitignore semantics. Mirrors pi's `ignoreStack`.
///
/// Engine parity:
/// - find (fd `--no-require-git`): gitignore applies whether or not the root
///   is inside a git repository (`require_git=false`);
/// - grep (rg): gitignore applies ONLY inside a git repository
///   (`require_git=true`);
/// - `node_modules` is NOT hard-ignored (only if gitignored);
/// - `.git` itself is always skipped;
/// - inside a repo, `.git/info/exclude` and the global `core.excludesFile`
///   apply, as do `.gitignore` files between the repo root and the search root.
pub(crate) struct IgnoreStack {
    root: String,
    /// Glob walk has loaded every visited directory from the pinned lease.
    /// Pattern lookup then uses only that cache.
    pinned: bool,
    /// Walk-root `.git/info/exclude` already read from the pinned lease.
    exclude_loaded: bool,
    fs: Option<Arc<crate::fs::PolicyFs>>,
    use_gitignore: bool,
    repo_root: String,
    /// When set, outer-repo ignore rules stop at nested repository boundaries.
    boundaries: bool,
    static_sources: Vec<IgnoreSource>,
    loaded: HashMap<String, Vec<IgnorePattern>>,
    git_dir: HashMap<String, bool>,
}

impl IgnoreStack {
    pub(crate) async fn new(
        generation: Arc<ApplicationGeneration>,
        root: &str,
        require_git: bool,
        respect_nested_repos: bool,
    ) -> Result<IgnoreStack> {
        let mut s = IgnoreStack {
            fs: Some(generation.sandbox().fs().confined()?.clone()),
            root: root.to_string(),
            pinned: false,
            exclude_loaded: false,
            use_gitignore: false,
            repo_root: String::new(),
            boundaries: false,
            static_sources: Vec::new(),
            loaded: HashMap::new(),
            git_dir: HashMap::new(),
        };
        s.repo_root = s.find_repo_root(root)?;
        s.use_gitignore = !require_git || !s.repo_root.is_empty();
        // Nested-repo boundaries only apply in git-aware mode (search root inside
        // a repo). Under `--no-require-git` outside a repo, fd ignores boundaries.
        s.boundaries = respect_nested_repos && !s.repo_root.is_empty();
        if !s.repo_root.is_empty() && s.use_gitignore {
            // Lowest precedence first; later sources win on conflicts.
            let g = global_excludes_path(generation, &s.repo_root).await;
            if !g.is_empty() {
                if let Some(data) = s.read_global_excludes(Path::new(&g))? {
                    s.static_sources.push(IgnoreSource {
                        base_abs: s.repo_root.clone(),
                        pats: parse_gitignore(&data),
                        boundary_exempt: true,
                    });
                }
            }
            let fs = s
                .fs
                .clone()
                .ok_or_else(|| anyhow::anyhow!("ignore stack has no filesystem"))?;
            if let Some(directory) = fs.resolve_glob_target(Path::new(&s.repo_root))? {
                if directory.entry_kind_relative(
                    &fs,
                    Path::new(&s.repo_root),
                    Path::new(".git"),
                )? == Some(crate::fs::EntryKind::Directory)
                {
                    if let Some(data) = directory.read_optional_relative(
                        &fs,
                        Path::new(&s.repo_root),
                        Path::new(".git/info/exclude"),
                        GITIGNORE_READ_LIMIT,
                    )? {
                        s.static_sources.push(IgnoreSource {
                            base_abs: s.repo_root.clone(),
                            pats: parse_gitignore(&data),
                            boundary_exempt: false,
                        });
                    }
                }
                s.exclude_loaded = true;
            }
            // Parents of the search root through the repo root. The walk root's
            // own `.gitignore` is loaded only by `load_pinned_directory`.
            if s.root != s.repo_root {
                let mut ancs: Vec<String> = Vec::new();
                let mut dir = parent_dir(&s.root);
                loop {
                    ancs.push(dir.clone());
                    if dir == s.repo_root || parent_dir(&dir) == dir {
                        break;
                    }
                    dir = parent_dir(&dir);
                }
                for i in (0..ancs.len()).rev() {
                    let lexical = PathBuf::from(&ancs[i]);
                    let directory = match fs.resolve_glob_target(&lexical) {
                        Ok(Some(directory)) => directory,
                        Ok(None) => continue,
                        Err(error)
                            if is_not_found(&error)
                                || matches!(error, crate::fs::FsError::OutsideCapability) =>
                        {
                            continue;
                        }
                        Err(error) => return Err(anyhow::anyhow!(error)),
                    };
                    if let Some(data) = directory.read_optional_relative(
                        &fs,
                        &lexical,
                        Path::new(".gitignore"),
                        GITIGNORE_READ_LIMIT,
                    )? {
                        s.static_sources.push(IgnoreSource {
                            base_abs: ancs[i].clone(),
                            pats: parse_gitignore(&data),
                            boundary_exempt: false,
                        });
                    }
                }
            }
        }
        Ok(s)
    }

    pub(crate) fn without_generation(
        root: &str,
        require_git: bool,
        respect_nested_repos: bool,
    ) -> IgnoreStack {
        let mut stack = IgnoreStack::without_gitignore(root);
        stack.use_gitignore = !require_git;
        stack.boundaries = respect_nested_repos && !stack.repo_root.is_empty();
        stack
    }

    /// Builds a stack that never applies gitignore rules (still skips `.git`
    /// via [`Self::ignored`]).
    pub(crate) fn without_gitignore(root: &str) -> IgnoreStack {
        IgnoreStack {
            fs: None,
            root: root.to_string(),
            pinned: false,
            exclude_loaded: false,
            use_gitignore: false,
            repo_root: String::new(),
            boundaries: false,
            static_sources: Vec::new(),
            loaded: HashMap::new(),
            git_dir: HashMap::new(),
        }
    }


    /// Nofollow: missing and symlink children are absent; other errors propagate.
    pub(crate) fn load_pinned_directory(
        &mut self,
        fs: &Arc<crate::fs::PolicyFs>,
        directory: &crate::fs::PolicyDirectory,
        lexical: &Path,
        relative: &Path,
    ) -> Result<()> {
        self.pinned = true;
        if !self.use_gitignore {
            return Ok(());
        }
        let rel_dir = relative.to_string_lossy().replace('\\', "/");
        if !self.loaded.contains_key(&rel_dir) {
            let patterns = match directory.read_optional_relative(
                fs,
                lexical,
                Path::new(".gitignore"),
                GITIGNORE_READ_LIMIT,
            )? {
                Some(data) => parse_gitignore(&data),
                None => Vec::new(),
            };
            self.loaded.insert(rel_dir.clone(), patterns);
        }
        if !self.git_dir.contains_key(&rel_dir) {
            let kind = directory.entry_kind_relative(fs, lexical, Path::new(".git"))?;
            self.git_dir.insert(rel_dir.clone(), kind.is_some());
            if rel_dir.is_empty()
                && !self.exclude_loaded
                && matches!(kind, Some(crate::fs::EntryKind::Directory))
            {
                if let Some(data) = directory.read_optional_relative(
                    fs,
                    lexical,
                    Path::new(".git/info/exclude"),
                    GITIGNORE_READ_LIMIT,
                )? {
                    self.static_sources.push(IgnoreSource {
                        base_abs: self.root.clone(),
                        pats: parse_gitignore(&data),
                        boundary_exempt: false,
                    });
                }
                self.exclude_loaded = true;
            }
        }
        Ok(())
    }

    /// Reports whether the root-relative dir contains a `.git` entry (a
    /// repository boundary). Results are cached.
    fn has_git_dir(&mut self, rel_dir: &str) -> bool {
        if let Some(&v) = self.git_dir.get(rel_dir) {
            return v;
        }
        if self.pinned {
            return false;
        }
        let v = self.metadata_exists(&self.lexical(rel_dir).join(".git"));
        self.git_dir.insert(rel_dir.to_string(), v);
        v
    }

    /// Reports whether a source rooted at `base_rel` is separated from `rel`
    /// by a nested repository: some directory strictly below `base_rel` and
    /// at-or-above `rel`'s own directory holds a `.git`.
    fn crosses_nested_boundary(&mut self, base_rel: &str, rel: &str) -> bool {
        for dir in ancestor_dirs(rel) {
            if dir == base_rel {
                continue;
            }
            // strict descendant of base_rel?
            let is_descendant = if base_rel.is_empty() {
                !dir.is_empty()
            } else {
                dir.starts_with(&format!("{base_rel}/"))
            };
            if !is_descendant {
                continue;
            }
            if self.has_git_dir(&dir) {
                return true;
            }
        }
        false
    }

    /// Loads (lazily) the `.gitignore` in the given root-relative dir.
    fn patterns_for(&mut self, rel_dir: &str) -> Result<Vec<IgnorePattern>> {
        if let Some(patterns) = self.loaded.get(rel_dir) {
            return Ok(patterns.clone());
        }
        if self.pinned {
            // The walk loads each directory before descent. A miss here is a
            // directory this stack was not handed, not a swallowed read.
            anyhow::bail!("pinned ignore directory was not loaded: {rel_dir}");
        }
        let patterns = self
            .try_read_optional(&self.lexical(rel_dir).join(".gitignore"))?
            .map(|data| parse_gitignore(&data))
            .unwrap_or_default();
        self.loaded.insert(rel_dir.to_string(), patterns.clone());
        Ok(patterns)
    }

    fn lexical(&self, rel_dir: &str) -> PathBuf {
        let relative = rel_dir.replace('\\', "/");
        if relative.is_empty() {
            PathBuf::from(&self.root)
        } else {
            PathBuf::from(&self.root).join(relative)
        }
    }

    fn metadata_exists(&self, path: &Path) -> bool {
        self.fs.as_ref().is_some_and(|fs| fs.metadata(path).is_ok())
    }

    fn try_read_optional(&self, path: &Path) -> Result<Option<Vec<u8>>> {
        if self.pinned {
            anyhow::bail!(
                "pinned ignore file must be read from the opened directory: {}",
                path.display()
            );
        }
        let Some(fs) = &self.fs else {
            return Ok(None);
        };
        match fs.read_bounded(path, GITIGNORE_READ_LIMIT) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if is_not_found(&error) => Ok(None),
            Err(error) => Err(anyhow::anyhow!(error)),
        }
    }


    fn read_global_excludes(&self, path: &Path) -> Result<Option<Vec<u8>>> {
        let Some(fs) = &self.fs else {
            return Ok(None);
        };
        match fs.read_bounded(path, GITIGNORE_READ_LIMIT) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error)
                if is_not_found(&error)
                    || matches!(
                        error,
                        crate::fs::FsError::OutsideCapability | crate::fs::FsError::Denied
                    ) =>
            {
                Ok(None)
            }
            Err(error) => Err(anyhow::Error::new(error).context("reading global excludes")),
        }
    }

    fn find_repo_root(&self, dir: &str) -> Result<String> {
        let Some(fs) = self.fs.clone() else {
            return Ok(String::new());
        };
        let mut dir = dir.to_string();
        loop {
            match fs.resolve_glob_target(Path::new(&dir)) {
                Ok(Some(directory)) => {
                    if directory
                        .entry_kind_relative(&fs, Path::new(&dir), Path::new(".git"))?
                        .is_some()
                    {
                        return Ok(dir);
                    }
                }
                Ok(None) => {}
                Err(error)
                    if is_not_found(&error)
                        || matches!(error, crate::fs::FsError::OutsideCapability) => {}
                Err(error) => return Err(anyhow::anyhow!(error)),
            }
            let parent = parent_dir(&dir);
            if parent == dir {
                return Ok(String::new());
            }
            dir = parent;
        }
    }

    /// Reports whether the path is ignored.
    pub(crate) fn ignored(&mut self, abs: &str, rel: &str, is_dir: bool) -> Result<bool> {
        let rel = rel.replace('\\', "/");
        // .git itself is always skipped.
        if Path::new(&rel).file_name().and_then(|s| s.to_str()) == Some(".git") {
            return Ok(true);
        }
        if !self.use_gitignore {
            return Ok(false);
        }

        let mut result = false;
        let statics = self.static_sources.clone();
        for src in &statics {
            // Repo-specific outer sources stop at a nested-repo boundary; global
            // excludes (boundaryExempt) carry across.
            if self.boundaries && !src.boundary_exempt && self.crosses_nested_boundary("", &rel) {
                continue;
            }
            let rel_to_base = rel_path(&src.base_abs, abs);
            if let Some(rts) = rel_to_base {
                for p in &src.pats {
                    if gitignore_match(p, &rts, is_dir) {
                        result = !p.negated;
                    }
                }
            }
        }
        for dir in ancestor_dirs(&rel) {
            // A .gitignore in an outer repo must not apply once a nested
            // repository begins below it.
            if self.boundaries && self.crosses_nested_boundary(&dir, &rel) {
                continue;
            }
            let rel_to_dir = if dir.is_empty() {
                rel.clone()
            } else {
                rel.strip_prefix(&format!("{dir}/"))
                    .unwrap_or(&rel)
                    .to_string()
            };
            let pats = self.patterns_for(&dir)?;
            for p in &pats {
                if gitignore_match(p, &rel_to_dir, is_dir) {
                    result = !p.negated;
                }
            }
        }
        Ok(result)
    }
}

const GITIGNORE_READ_LIMIT: u64 = 8 * 1024 * 1024;



fn is_not_found(error: &crate::fs::FsError) -> bool {
    matches!(
        error,
        crate::fs::FsError::System {
            kind: io::ErrorKind::NotFound,
            ..
        }
    )
}

fn parent_dir(dir: &str) -> String {
    match Path::new(dir).parent() {
        Some(p) if p.as_os_str().is_empty() => dir.to_string(),
        Some(p) => p.to_string_lossy().into_owned(),
        None => dir.to_string(),
    }
}

/// Resolves git's global excludes file through the governed RepositoryHelper,
/// falling back to the conventional XDG/home locations.
async fn global_excludes_path(generation: Arc<ApplicationGeneration>, repo_root: &str) -> String {
    if let Ok(git) = run_trusted_tool_probe(&generation, TrustedToolProbe::Git) {
        let output = GovernedProcessContext::new(generation, None)
            .run(
                pi_sandbox::SpawnFamily::RepositoryHelper,
                GovernedProcessSpec {
                    program: git.as_path().to_path_buf(),
                    args: ["config", "--path", "--get", "core.excludesFile"]
                        .into_iter()
                        .map(str::to_owned)
                        .collect(),
                    cwd: Path::new(repo_root).to_path_buf(),
                    environment: BTreeMap::new(),
                    timeout: Duration::from_secs(5),
                    stdout_bytes: 16 * 1024,
                    stderr_bytes: 16 * 1024,
                    #[cfg(unix)]
                    mount_sources: Vec::new(),
                    #[cfg(unix)]
                    retained_executable: None,
                    #[cfg(unix)]
                    retained_working_directory: None,
                },
            )
            .await;
        if let Ok(output) = output
            && output.status == Some(0)
            && !output.timed_out
            && !output.cancelled
        {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            if !path.is_empty() {
                return path;
            }
        }
    }
    fallback_global_excludes_path()
}

fn fallback_global_excludes_path() -> String {
    if let Ok(x) = std::env::var("XDG_CONFIG_HOME")
        && !x.is_empty()
    {
        return format!("{x}/git/ignore");
    }
    if let Ok(home) = std::env::var("HOME")
        && !home.is_empty()
    {
        return format!("{home}/.config/git/ignore");
    }
    String::new()
}

/// Parses `.gitignore` data into rules (port of `parseGitignore`).
pub(crate) fn parse_gitignore(data: &[u8]) -> Vec<IgnorePattern> {
    let mut out = Vec::new();
    let text = String::from_utf8_lossy(data);
    for line in text.split('\n') {
        let line = line.trim_end_matches('\r');
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let mut neg = false;
        let mut t = trimmed;
        if let Some(rest) = t.strip_prefix('!') {
            neg = true;
            t = rest;
        }
        let dir_only = t.ends_with('/');
        t = t.strip_suffix('/').unwrap_or(t);
        let p = t.replace('\\', "/");
        let anchored = p.contains('/');
        let p = p.strip_prefix('/').map(|s| s.to_string()).unwrap_or(p);
        if p.is_empty() {
            continue;
        }
        out.push(IgnorePattern {
            pattern: p,
            anchored,
            dir_only,
            negated: neg,
        });
    }
    out
}

/// The chain of root-relative directories from root (`""`) down to (and
/// including) the directory containing `rel`.
fn ancestor_dirs(rel: &str) -> Vec<String> {
    let rel = rel.replace('\\', "/");
    let parts: Vec<&str> = rel.split('/').collect();
    let mut dirs = vec![String::new()];
    let mut cur = String::new();
    // All components except the last are directories that may hold .gitignore.
    for (i, part) in parts.iter().enumerate() {
        if i >= parts.len() - 1 {
            break;
        }
        if cur.is_empty() {
            cur = part.to_string();
        } else {
            cur = format!("{cur}/{part}");
        }
        dirs.push(cur.clone());
    }
    dirs
}

/// Computes `base`-relative path of `abs` (lexically), or `None` if `abs` is not
/// under `base`.
fn rel_path(base: &str, abs: &str) -> Option<String> {
    let base = base.replace('\\', "/");
    let abs = abs.replace('\\', "/");
    if abs == base {
        return Some(String::new());
    }
    let prefix = format!("{base}/");
    abs.strip_prefix(&prefix).map(|s| s.to_string())
}

/// Reports whether a pattern matches `rel_to_dir` (path relative to the
/// pattern's base directory) per gitignore semantics.
fn gitignore_match(p: &IgnorePattern, rel_to_dir: &str, is_dir: bool) -> bool {
    let rel_to_dir = rel_to_dir.replace('\\', "/");
    if p.anchored {
        if glob_match_path(&p.pattern, &rel_to_dir, false) {
            return !p.dir_only || is_dir;
        }
        // A pattern matching an ancestor directory ignores everything below it.
        let segs: Vec<&str> = rel_to_dir.split('/').collect();
        let mut prefix = String::new();
        for i in 0..segs.len() - 1 {
            if prefix.is_empty() {
                prefix = segs[i].to_string();
            } else {
                prefix = format!("{prefix}/{}", segs[i]);
            }
            if glob_match_path(&p.pattern, &prefix, false) {
                return true;
            }
        }
        return false;
    }
    // Unanchored: match against each path component; a hit on a non-final
    // component means the path is inside a matching directory.
    let segs: Vec<&str> = rel_to_dir.split('/').collect();
    for (i, seg) in segs.iter().enumerate() {
        if seg_match(&p.pattern, seg, false) {
            if i < segs.len() - 1 {
                return true;
            }
            return !p.dir_only || is_dir;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    struct GenerationFixture {
        _agent: tempfile::TempDir,
        _sessions: tempfile::TempDir,
        _resources: crate::ResourceManager,
        _session: crate::Session,
        _application: crate::Application,
        _faux: pi_ai::providers::FauxProviderRegistration,
        generation: Arc<ApplicationGeneration>,
    }

    async fn generation_fixture(cwd: &Path) -> GenerationFixture {
        let agent = tempfile::tempdir().expect("agent dir");
        let sessions = tempfile::tempdir().expect("session dir");
        // Explicit danger-full-access so the sandbox stages under the local
        // Bubblewrap (0.9.0) without trusted-launcher probe validation.
        fs::write(
            agent.path().join("settings.json"),
            r#"{"sandbox":{"mode":"danger-full-access"}}"#,
        )
        .expect("sandbox settings");
        let mut resource_options = crate::ResourceManagerOptions::new(cwd);
        resource_options.agent_dir = agent.path().to_path_buf();
        resource_options.project_trust_override = Some(true);
        resource_options.disable_extensions = true;
        resource_options.disable_skills = true;
        resource_options.disable_prompt_templates = true;
        resource_options.disable_themes = true;
        resource_options.disable_context_files = true;
        let resources = crate::ResourceManager::new(resource_options).expect("resources");
        let suffix = uuid::Uuid::new_v4().to_string();
        let api = format!("glob-{suffix}");
        let provider = format!("glob-{suffix}");
        let model = pi_ai::Model {
            id: format!("glob-{suffix}"),
            api: api.clone(),
            provider: provider.clone(),
            ..pi_ai::Model::default()
        };
        let faux =
            pi_ai::providers::register_faux_provider(pi_ai::providers::FauxProviderOptions {
                api,
                provider,
                models: vec![model.clone()],
                chunk_size: 1,
            });
        let workspace =
            crate::WorkspaceRoots::new(cwd, Vec::<std::path::PathBuf>::new()).expect("workspace");
        let session =
            crate::Session::new_with_additional_tools_filtered_discovery_workspace_and_uri(
                crate::SessionOptions {
                    model: Some(model),
                    cwd: cwd.to_path_buf(),
                    system_prompt: String::new(),
                    thinking_level: pi_agent::ThinkingLevel::Off,
                    api_key: String::new(),
                    compaction: None,
                    stream_options: Default::default(),
                    tools: Some(Vec::new()),
                    before_tool_call: None,
                    after_tool_call: None,
                    stream_fn: None,
                    auth_resolver: None,
                    auth_manager: None,
                },
                Vec::new(),
                crate::ToolSelection::default(),
                crate::ResourceDiscovery::Disabled,
                workspace,
                None,
            )
            .expect("session");
        session.set_session_dir(sessions.path().to_path_buf());
        session
            .attach_resources(resources.clone())
            .await
            .expect("attach resources");
        let application = crate::Application::from_runtime_candidate(
            crate::ApplicationRuntimeCandidate::new(session.clone()),
        )
        .await
        .expect("publish glob application");
        let generation = application.capture_generation();
        GenerationFixture {
            _agent: agent,
            _sessions: sessions,
            _resources: resources,
            _session: session,
            _application: application,
            _faux: faux,
            generation,
        }
    }

    fn tmp() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("pi-glob-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn governed_repository_helper_family_is_exact() {
        assert_eq!(
            pi_sandbox::SpawnFamily::RepositoryHelper.as_str(),
            "repository-helper"
        );
    }

    #[test]
    fn expand_braces_simple() {
        assert_eq!(expand_braces("a"), vec!["a".to_string()]);
        assert_eq!(
            expand_braces("{a,b}"),
            vec!["a".to_string(), "b".to_string()]
        );
        assert_eq!(
            expand_braces("x{a,b}y"),
            vec!["xay".to_string(), "xby".to_string()]
        );
    }

    #[test]
    fn path_match_star_and_class() {
        assert!(path_match("*.rs", "a.rs"));
        assert!(!path_match("*.rs", "a.txt"));
        assert!(path_match("[abc].rs", "b.rs"));
        assert!(path_match("[a-c].rs", "b.rs"));
        assert!(path_match("[!abc].rs", "x.rs") == false); // [!] not converted here
        assert!(path_match("[^abc].rs", "x.rs"));
        assert!(!path_match("[^abc].rs", "a.rs"));
        assert!(path_match("?.rs", "x.rs"));
        assert!(path_match("*", "anything"));
    }

    #[test]
    fn glob_match_double_star_crosses_slashes() {
        assert!(glob_match_path("**/*.ts", "a/b/c.ts", false));
        assert!(glob_match_path("src/**", "src/a/b", false));
        assert!(glob_match_path("a/**", "a/b", false));
        assert!(!glob_match_path("a/**", "a", false));
    }

    #[test]
    fn match_fd_glob_basename_no_slash() {
        assert!(match_fd_glob("*.rs", "src/main.rs", "/proj/src/main.rs"));
        assert!(!match_fd_glob("*.rs", "src/main.go", "/proj/src/main.go"));
    }

    #[test]
    fn match_fd_glob_full_path_with_slash() {
        assert!(match_fd_glob(
            "**/*.spec.ts",
            "src/a.spec.ts",
            "/proj/src/a.spec.ts"
        ));
        // smart-case: an all-lowercase pattern matches a differently-cased basename.
        assert!(match_fd_glob("readme", "README", "/proj/README"));
    }

    #[test]
    fn match_rg_glob_anchored() {
        assert!(match_rg_glob("*.ts", "a/b.ts"));
        assert!(match_rg_glob("src/*.ts", "src/a.ts"));
        assert!(!match_rg_glob("src/*.ts", "other/a.ts"));
    }

    #[test]
    fn parse_gitignore_basic() {
        let pats = parse_gitignore(b"# comment\nnode_modules\n*.log\n!important.log\n/dist/\n");
        assert_eq!(pats.len(), 4);
        assert!(pats[0].pattern == "node_modules" && !pats[0].anchored && !pats[0].dir_only);
        assert!(pats[1].pattern == "*.log" && !pats[1].anchored);
        assert!(pats[2].negated && pats[2].pattern == "important.log");
        assert!(pats[3].dir_only && pats[3].pattern == "dist" && pats[3].anchored);
    }

    #[tokio::test]
    async fn ignore_stack_skips_git_always() {
        let d = tmp();
        fs::write(d.join(".gitignore"), b("*.tmp\n")).unwrap();
        let fixture = generation_fixture(&d).await;
        let mut ig = IgnoreStack::new(fixture.generation, &d.to_string_lossy(), false, true)
            .await
            .expect("generation-bound ignore stack");
        // .git itself always skipped regardless of gitignore.
        assert!(ig.ignored(&d.join(".git").to_string_lossy(), ".git", true).unwrap());
        assert!(ig.ignored(&d.join("a.tmp").to_string_lossy(), "a.tmp", false).unwrap());
        assert!(!ig.ignored(&d.join("a.rs").to_string_lossy(), "a.rs", false).unwrap());
    }

    #[tokio::test]
    async fn global_excludes_over_read_limit_is_not_optional() {
        let d = tmp();
        let oversize = d.join("excludes");
        let file = fs::File::create(&oversize).expect("create excludes");
        file.set_len(GITIGNORE_READ_LIMIT + 1)
            .expect("sparse oversize excludes");
        drop(file);
        let fixture = generation_fixture(&d).await;
        let mut stack = IgnoreStack::without_gitignore(&d.to_string_lossy());
        stack.fs = Some(
            fixture
                .generation
                .sandbox()
                .fs()
                .confined()
                .expect("policy fs")
                .clone(),
        );
        let error = stack
            .read_global_excludes(&oversize)
            .expect_err("oversize global excludes must not be treated as absent");
        assert!(
            error
                .downcast_ref::<crate::fs::FsError>()
                .is_some_and(|fs| matches!(fs, crate::fs::FsError::ReadLimitExceeded { .. })),
            "expected ReadLimitExceeded, got {error:#}"
        );
        fs::write(&oversize, b"*.log\n").unwrap();
        let bytes = stack
            .read_global_excludes(&oversize)
            .expect("in-limit global excludes")
            .expect("present file");
        assert_eq!(bytes, b"*.log\n");
    }


    #[tokio::test]
    async fn ignore_stack_grep_requires_git() {
        let d = tmp();
        fs::write(d.join(".gitignore"), b("ignored.txt\n")).unwrap();
        // No .git → requireGit=true means gitignore does NOT apply.
        let fixture = generation_fixture(&d).await;
        let mut ig = IgnoreStack::new(
            fixture.generation.clone(),
            &d.to_string_lossy(),
            true,
            false,
        )
        .await
        .expect("generation-bound ignore stack");
        assert!(!ig
            .ignored(&d.join("ignored.txt").to_string_lossy(), "ignored.txt", false)
            .unwrap());
        // Add a valid repository marker → now in a repo, gitignore applies.
        fs::create_dir(d.join(".git")).unwrap();
        fs::write(d.join(".git/HEAD"), b"ref: refs/heads/test\n").unwrap();
        let mut ig2 = IgnoreStack::new(fixture.generation, &d.to_string_lossy(), true, false)
            .await
            .expect("generation-bound repository ignore stack");
        assert!(ig2
            .ignored(&d.join("ignored.txt").to_string_lossy(), "ignored.txt", false)
            .unwrap());
    }

    #[tokio::test]
    async fn ignore_stack_negation() {
        let d = tmp();
        fs::create_dir(d.join(".git")).unwrap();
        fs::write(d.join(".git/HEAD"), b"ref: refs/heads/test\n").unwrap();
        fs::write(d.join(".gitignore"), b"*.log\n!important.log\n").unwrap();
        let fixture = generation_fixture(&d).await;
        let mut ig = IgnoreStack::new(fixture.generation, &d.to_string_lossy(), false, true)
            .await
            .expect("generation-bound ignore stack");
        assert!(ig.ignored(&d.join("a.log").to_string_lossy(), "a.log", false).unwrap());
        assert!(!ig
            .ignored(
                &d.join("important.log").to_string_lossy(),
                "important.log",
                false
            )
            .unwrap());
    }

    fn b(s: &str) -> Vec<u8> {
        s.as_bytes().to_vec()
    }

    // Silence unused import when only some tests reference Path.
    #[allow(dead_code)]
    fn _use_path(_: &Path) {}
}
