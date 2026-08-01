//! Glob primitives and the gitignore engine (port of pi's `coding/glob.go`).
//!
//! Shared by the fd-style find matcher (`match_fd_glob`), the rg-style grep
//! matcher (`match_rg_glob`), and the hierarchical gitignore engine
//! (`IgnoreStack`). The gitignore engine is a faithful pure-Rust port so that
//! find's `requireGit=false` (gitignore applies outside a repo, fd
//! `--no-require-git`) and grep's `requireGit=true` (rg) semantics match pi
//! exactly, including nested-repo boundaries and `.git` always being skipped.

use std::collections::HashMap;
use std::path::Path;

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
            Path::new(rel).file_name().and_then(|s| s.to_str()).unwrap_or(""),
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
            Path::new(&rel).file_name().and_then(|s| s.to_str()).unwrap_or(""),
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
    use_gitignore: bool,
    repo_root: String,
    /// When set, outer-repo ignore rules stop at nested repository boundaries.
    boundaries: bool,
    static_sources: Vec<IgnoreSource>,
    loaded: HashMap<String, Vec<IgnorePattern>>,
    git_dir: HashMap<String, bool>,
}

impl IgnoreStack {
    pub(crate) fn new(root: &str, require_git: bool, respect_nested_repos: bool) -> IgnoreStack {
        let mut s = IgnoreStack {
            root: root.to_string(),
            use_gitignore: false,
            repo_root: String::new(),
            boundaries: false,
            static_sources: Vec::new(),
            loaded: HashMap::new(),
            git_dir: HashMap::new(),
        };
        s.repo_root = find_repo_root(root);
        s.use_gitignore = !require_git || !s.repo_root.is_empty();
        // Nested-repo boundaries only apply in git-aware mode (search root inside
        // a repo). Under `--no-require-git` outside a repo, fd ignores boundaries.
        s.boundaries = respect_nested_repos && !s.repo_root.is_empty();
        if !s.repo_root.is_empty() && s.use_gitignore {
            // Lowest precedence first; later sources win on conflicts.
            let g = global_excludes_path();
            if !g.is_empty() {
                if let Ok(data) = std::fs::read(&g) {
                    s.static_sources.push(IgnoreSource {
                        base_abs: s.repo_root.clone(),
                        pats: parse_gitignore(&data),
                        boundary_exempt: true,
                    });
                }
            }
            let info_exclude = format!("{}/.git/info/exclude", s.repo_root);
            if let Ok(data) = std::fs::read(&info_exclude) {
                s.static_sources.push(IgnoreSource {
                    base_abs: s.repo_root.clone(),
                    pats: parse_gitignore(&data),
                    boundary_exempt: false,
                });
            }
            // .gitignore files in ancestors of the search root (repo root downward).
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
                    if let Ok(data) = std::fs::read(format!("{}/.gitignore", ancs[i])) {
                        s.static_sources.push(IgnoreSource {
                            base_abs: ancs[i].clone(),
                            pats: parse_gitignore(&data),
                            boundary_exempt: false,
                        });
                    }
                }
            }
        }
        s
    }

    /// Builds a stack that never applies gitignore rules (still skips `.git`
    /// via [`Self::ignored`]).
    pub(crate) fn without_gitignore(root: &str) -> IgnoreStack {
        IgnoreStack {
            root: root.to_string(),
            use_gitignore: false,
            repo_root: String::new(),
            boundaries: false,
            static_sources: Vec::new(),
            loaded: HashMap::new(),
            git_dir: HashMap::new(),
        }
    }

    /// Reports whether the root-relative dir contains a `.git` entry (a
    /// repository boundary). Results are cached.
    fn has_git_dir(&mut self, rel_dir: &str) -> bool {
        if let Some(&v) = self.git_dir.get(rel_dir) {
            return v;
        }
        let abs = if rel_dir.is_empty() {
            self.root.clone()
        } else {
            format!("{}/{}", self.root, rel_dir.replace('\\', "/"))
        };
        let v = std::fs::metadata(format!("{abs}/.git")).is_ok();
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
    fn patterns_for(&mut self, rel_dir: &str) -> &Vec<IgnorePattern> {
        let abs = if rel_dir.is_empty() {
            self.root.clone()
        } else {
            format!("{}/{}", self.root, rel_dir.replace('\\', "/"))
        };
        let entry = self.loaded.entry(rel_dir.to_string()).or_insert_with(|| {
            std::fs::read(format!("{abs}/.gitignore"))
                .map(|d| parse_gitignore(&d))
                .unwrap_or_default()
        });
        entry
    }

    /// Reports whether the path is ignored.
    pub(crate) fn ignored(&mut self, abs: &str, rel: &str, is_dir: bool) -> bool {
        let rel = rel.replace('\\', "/");
        // .git itself is always skipped.
        if Path::new(&rel).file_name().and_then(|s| s.to_str()) == Some(".git") {
            return true;
        }
        if !self.use_gitignore {
            return false;
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
                rel.strip_prefix(&format!("{dir}/")).unwrap_or(&rel).to_string()
            };
            let pats = self.patterns_for(&dir).clone();
            for p in &pats {
                if gitignore_match(p, &rel_to_dir, is_dir) {
                    result = !p.negated;
                }
            }
        }
        result
    }
}

fn parent_dir(dir: &str) -> String {
    match Path::new(dir).parent() {
        Some(p) if p.as_os_str().is_empty() => dir.to_string(),
        Some(p) => p.to_string_lossy().into_owned(),
        None => dir.to_string(),
    }
}

/// Walks up from `dir` looking for a `.git` entry (dir or file).
pub(crate) fn find_repo_root(dir: &str) -> String {
    let mut dir = dir.to_string();
    loop {
        if std::fs::metadata(format!("{dir}/.git")).is_ok() {
            return dir;
        }
        let parent = parent_dir(&dir);
        if parent == dir {
            return String::new();
        }
        dir = parent;
    }
}

/// Resolves git's global excludes file: `core.excludesFile` if configured, else
/// `$XDG_CONFIG_HOME/git/ignore`, else `~/.config/git/ignore`.
fn global_excludes_path() -> String {
    if let Ok(out) = std::process::Command::new("git")
        .args(["config", "--path", "--get", "core.excludesFile"])
        .output()
    {
        if let Ok(s) = String::from_utf8(out.stdout) {
            let p = s.trim().to_string();
            if !p.is_empty() {
                return p;
            }
        }
    }
    if let Ok(x) = std::env::var("XDG_CONFIG_HOME") {
        if !x.is_empty() {
            return format!("{x}/git/ignore");
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            return format!("{home}/.config/git/ignore");
        }
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

    fn tmp() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("pi-glob-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn expand_braces_simple() {
        assert_eq!(expand_braces("a"), vec!["a".to_string()]);
        assert_eq!(expand_braces("{a,b}"), vec!["a".to_string(), "b".to_string()]);
        assert_eq!(expand_braces("x{a,b}y"), vec!["xay".to_string(), "xby".to_string()]);
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
        assert!(match_fd_glob("**/*.spec.ts", "src/a.spec.ts", "/proj/src/a.spec.ts"));
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

    #[test]
    fn ignore_stack_skips_git_always() {
        let d = tmp();
        fs::write(d.join(".gitignore"), b("*.tmp\n")).unwrap();
        let mut ig = IgnoreStack::new(&d.to_string_lossy(), false, true);
        // .git itself always skipped regardless of gitignore.
        assert!(ig.ignored(&d.join(".git").to_string_lossy(), ".git", true));
        // ignored by *.tmp
        assert!(ig.ignored(&d.join("a.tmp").to_string_lossy(), "a.tmp", false));
        // not ignored
        assert!(!ig.ignored(&d.join("a.rs").to_string_lossy(), "a.rs", false));
    }

    #[test]
    fn ignore_stack_grep_requires_git() {
        let d = tmp();
        fs::write(d.join(".gitignore"), b("ignored.txt\n")).unwrap();
        // No .git → requireGit=true means gitignore does NOT apply.
        let mut ig = IgnoreStack::new(&d.to_string_lossy(), true, false);
        assert!(!ig.ignored(&d.join("ignored.txt").to_string_lossy(), "ignored.txt", false));
        // Add a .git marker → now in a repo, gitignore applies.
        fs::write(d.join(".git"), b("")).unwrap();
        let mut ig2 = IgnoreStack::new(&d.to_string_lossy(), true, false);
        assert!(ig2.ignored(&d.join("ignored.txt").to_string_lossy(), "ignored.txt", false));
    }

    #[test]
    fn ignore_stack_negation() {
        let d = tmp();
        fs::write(d.join(".git"), b("")).unwrap();
        fs::write(d.join(".gitignore"), b("*.log\n!important.log\n")).unwrap();
        let mut ig = IgnoreStack::new(&d.to_string_lossy(), false, true);
        assert!(ig.ignored(&d.join("a.log").to_string_lossy(), "a.log", false));
        assert!(!ig.ignored(&d.join("important.log").to_string_lossy(), "important.log", false));
    }

    fn b(s: &str) -> Vec<u8> {
        s.as_bytes().to_vec()
    }

    // Silence unused import when only some tests reference Path.
    #[allow(dead_code)]
    fn _use_path(_: &Path) {}
}