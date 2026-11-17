//! Authorized, bounded one-hop dependency maps. Purpose text is verbatim source
//! documentation, never a generated description. Resolution is deliberately
//! conservative: unsupported Rust contexts remain unresolved rather than guessed.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use pi_coding::{Application, FsError};
use pi_coding::application::ApplicationGeneration;
use pi_coding::dependency_map::{DeclarationEntry, DependencyEdge, EdgeKind, MapLanguage, ModuleDeclaration, ParseStatus, SourceDependencies, extract_dependencies};
use serde::Serialize;

use crate::file_explorer::{self, FileExplorerError};
use crate::models_config::strip_json_comments_padded;

pub const MAX_SCAN_FILES: usize = 5_000;
pub const MAX_SOURCE_BYTES: usize = 1024 * 1024;

/// Directory scope file budget stays 80. Do not raise.
const MAX_DIRECTORY_FILES: usize = 80;

#[derive(Debug, Clone, Serialize)]
pub struct MapNode {
    pub path: String,
    pub language: MapLanguage,
    pub parse: ParseStatus,
    pub declarations: Vec<DeclarationEntry>,
    /// Verbatim declaration documentation with its declaration's source line.
    pub purpose: Vec<SourcePurpose>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SourcePurpose {
    pub line: usize,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MapTarget {
    File { path: String },
    External { specifier: String },
    /// A Go package directory: `"."` at the module root, otherwise a
    /// canonical workspace-relative POSIX directory. Never a file.
    Package { dir: String },
    Unresolved { reason: String },
}

#[derive(Debug, Clone, Serialize)]
pub struct MapEdge {
    pub source: String,
    pub line: usize,
    pub specifier: String,
    pub kind: EdgeKind,
    pub target: MapTarget,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct MapCoverage {
    pub scanned_files: usize,
    pub unsupported_files: usize,
    pub unreadable_files: usize,
    pub unreadable_directories: usize,
    pub incomplete_files: usize,
    pub dynamic_omitted: usize,
    pub truncated: bool,
    /// Explorer listing intentionally omits ignored entries and symlinks and
    /// cannot report entries whose metadata failed. Reverse coverage is scoped
    /// to visible entries, not a proof of all filesystem dependents.
    pub visible_entries_only: bool,
    pub incomplete: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct DependencyMapResponse {
    pub path: String,
    pub nodes: Vec<MapNode>,
    pub outgoing: Vec<MapEdge>,
    pub incoming: Vec<MapEdge>,
    pub coverage: MapCoverage,
}

/// Build a one-hop map using only the captured generation's filesystem authority.
/// The final serialized payload is capped at 256 KiB; oversized maps fail rather
/// than silently dropping evidence. Listing and source budgets are independent.
pub fn dependency_map(app: &Application, path: &str) -> Result<DependencyMapResponse> {
    let session = app.session();
    let root = session.cwd();
    let generation = app.capture_generation();
    // A directory path is the opened folder's graph: one listing, edges
    // only among the files in that directory. A file path still fails this
    // open and falls through to the bounded file-map scan.
    if let Ok(listing) = file_explorer::list_with_generation(&generation, root, path) {
        return directory_graph(&generation, root, path, &listing);
    }
    let foundry = load_foundry_default(&generation, root)?;
    let (_, path) = file_explorer::resolve_scoped_path(root, path, false)?;
    let selected = read_source(&generation, root, &path)?;
    let mut sources = BTreeMap::new();
    let mut coverage = MapCoverage { visible_entries_only: true, incomplete: true, ..Default::default() };
    record_source(&mut coverage, &selected.source);
    let selected_go_clause = if selected.source.language == MapLanguage::Go {
        let text = std::str::from_utf8(&selected.bytes)
            .with_context(|| format!("selected source {} is not UTF-8", selected.relative))?;
        Some(go_package_clause(text))
    } else {
        None
    };
    sources.insert(selected.relative, selected.source);
    let mut directories = VecDeque::from([String::new()]);
    let mut visited = 0usize;
    let mut files_seen = 1usize;
    while let Some(directory) = directories.pop_front() {
        if visited >= MAX_SCAN_FILES {
            coverage.truncated = true;
            break;
        }
        visited += 1;
        // Explorer's public error mapping does not retain enough information to
        // classify omissions safely. Any listing failure aborts this map.
        let listing = file_explorer::list_with_generation(&generation, root, &directory)
            .with_context(|| format!("dependency directory unavailable: {directory}"))?;
        coverage.truncated |= listing.truncated;
        for entry in listing.entries {
            if entry.kind == "dir" {
                if directories.len() < MAX_SCAN_FILES {
                    directories.push_back(entry.path);
                } else {
                    coverage.truncated = true;
                }
                continue;
            }
            if entry.path == path { continue; }
            if files_seen >= MAX_SCAN_FILES {
                coverage.truncated = true;
                continue;
            }
            files_seen += 1;
            if extract_dependencies(&entry.path, "").language == MapLanguage::Unsupported {
                coverage.unsupported_files += 1;
                continue;
            }
            match read_source(&generation, root, &entry.path) {
                Ok(selected) => {
                    record_source(&mut coverage, &selected.source);
                    sources.insert(entry.path, selected.source);
                }
                Err(error) if allowed_source_omission(&error) => coverage.unreadable_files += 1,
                Err(error) => return Err(error),
            }
        }
        if files_seen >= MAX_SCAN_FILES && !directories.is_empty() {
            coverage.truncated = true;
            break;
        }
    }
    let probe = |candidate: &str| probe_candidate(&generation, root, candidate);
    let mut caches = ResolutionCaches::default();
    if let Some(clause) = selected_go_clause {
        caches.go_member_clauses.insert(path.to_owned(), clause);
    }
    let mut response = assemble(&path, root, &sources, foundry.as_ref(), coverage, &probe, &mut caches, &generation)?;
    // This is a payload bound, not an evidence truncation policy.
    response.nodes.sort_by(|a, b| a.path.cmp(&b.path));
    if serde_json::to_vec(&response)?.len() > 256 * 1024 {
        return Err(anyhow!("dependency map exceeds response limit"));
    }
    Ok(response)
}

/// `[profile.default]` of the workspace's `foundry.toml`, loaded once per map
/// request and shared by directory scoping and edge resolution.
#[derive(Debug, Clone)]
pub(crate) struct FoundryDefault {
    /// `[profile.default].src` when present, otherwise "src".
    pub src: String,
    /// Longest-prefix map. Empty only when the key is absent on a valid default profile.
    pub remappings: BTreeMap<String, String>,
}

/// Read only `root/foundry.toml` through the captured generation. A missing
/// file is the only `Ok(None)`; a malformed table, a bad or duplicated
/// remapping, or any other read failure is a context-carrying `Err`, so
/// resolution fails closed instead of guessing `lib/`, other profiles, or
/// parent directories.
pub(crate) fn load_foundry_default(
    generation: &ApplicationGeneration,
    root: &Path,
) -> Result<Option<FoundryDefault>> {
    let profile_path = root.join("foundry.toml");
    let described = format!("foundry profile {}", profile_path.display());
    let bytes = match generation.read_file_bounded(&profile_path, MAX_SOURCE_BYTES as u64) {
        Ok(bytes) => bytes,
        Err(error) => return match error.downcast_ref::<FsError>() {
            Some(FsError::System { kind: std::io::ErrorKind::NotFound, .. }) => Ok(None),
            _ => Err(error.context(format!("{described} unavailable"))),
        },
    };
    let text = std::str::from_utf8(&bytes).with_context(|| format!("{described} is not UTF-8"))?;
    let table: toml::Table =
        toml::from_str(text).with_context(|| format!("{described} is not valid TOML"))?;
    let default = table
        .get("profile")
        .and_then(|profiles| profiles.as_table())
        .and_then(|profiles| profiles.get("default"))
        .and_then(|profile| profile.as_table())
        .with_context(|| format!("{described} has no [profile.default]"))?;
    let src = match default.get("src") {
        None => "src".to_owned(),
        Some(value) => value
            .as_str()
            .map(str::to_owned)
            .with_context(|| format!("{described} has a non-string src"))?,
    };
    let mut remappings = BTreeMap::new();
    if let Some(entries) = default.get("remappings") {
        let array = entries
            .as_array()
            .with_context(|| format!("{described} remappings is not an array"))?;
        for entry in array {
            let text = entry
                .as_str()
                .with_context(|| format!("{described} remapping is not a string"))?;
            let (prefix, target) = text
                .split_once('=')
                .with_context(|| format!("{described} remapping is not key=value: {text:?}"))?;
            if prefix.is_empty() {
                return Err(anyhow!("{described} remapping prefix is empty: {text:?}"));
            }
            if remappings.insert(prefix.to_owned(), target.to_owned()).is_some() {
                return Err(anyhow!("{described} remapping prefix is duplicated: {prefix:?}"));
            }
        }
    }
    Ok(Some(FoundryDefault { src, remappings }))
}

/// One graph of one source file from an already-read [`SelectedSource`]:
/// classify its outgoing edges and report nothing else. The selected bytes
/// are never read again; auxiliary evidence (probes, listings, package
/// members) may be read through the same captured generation. Reverse
/// coverage is never taken, so `coverage.incomplete` stays true.
pub(crate) fn one_hop_dependencies(
    generation: &ApplicationGeneration,
    root: &Path,
    selected: SelectedSource,
) -> Result<DependencyMapResponse> {
    let foundry = load_foundry_default(generation, root)?;
    let mut coverage = MapCoverage {
        visible_entries_only: true,
        incomplete: true,
        ..MapCoverage::default()
    };
    record_source(&mut coverage, &selected.source);
    let probe = |candidate: &str| probe_candidate(generation, root, candidate);
    let mut caches = ResolutionCaches::default();
    if selected.source.language == MapLanguage::Go {
        // The selected capture is reused by package proofs; its one read
        // counts once in the shared proof budget.
        let text = std::str::from_utf8(&selected.bytes)
            .with_context(|| format!("selected source {} is not UTF-8", selected.relative))?;
        caches.go_member_clauses.insert(selected.relative.clone(), go_package_clause(text));
        caches.go_member_reads = 1;
    }
    let mut response = assemble_one_hop(&selected.relative, root, &selected.source, foundry.as_ref(), coverage, &probe, &mut caches, generation)?;
    response.nodes.sort_by(|left, right| left.path.cmp(&right.path));
    if serde_json::to_vec(&response)?.len() > 256 * 1024 {
        return Err(anyhow!("dependency map exceeds response limit"));
    }
    Ok(response)
}

/// A Foundry default profile scopes directory recursion: the request must be
/// the workspace root, the configured `src`, or a subdirectory of it. Every
/// other directory stays a single listing level.
fn foundry_scope(foundry: Option<&FoundryDefault>, relative: &str) -> Option<String> {
    let src = &foundry?.src;
    if relative.is_empty() || relative == src {
        return Some(src.clone());
    }
    relative
        .strip_prefix(src)
        .and_then(|rest| rest.strip_prefix('/'))
        .map(|_| relative.to_owned())
}

/// One graph of the opened directory. Inside a Foundry default scope (the
/// workspace root, `[profile.default].src`, or a subdirectory of it) the
/// source tree is recursed up to the 80-source budget; every other directory
/// stays one listing level. Edge targets outside the scope stay classified
/// edges without becoming nodes, so the opened folder keeps its imports
/// visible without pulling foreign trees into the node picture.
fn directory_graph(
    generation: &ApplicationGeneration,
    root: &Path,
    path: &str,
    listing: &file_explorer::ListResponse,
) -> Result<DependencyMapResponse> {
    let foundry = load_foundry_default(generation, root)?;
    let (_, relative) = file_explorer::resolve_scoped_path(root, path, true)?;
    let mut sources = BTreeMap::new();
    let mut coverage = MapCoverage {
        visible_entries_only: true,
        incomplete: true,
        ..MapCoverage::default()
    };
    match foundry_scope(foundry.as_ref(), &relative) {
        Some(scope_root) => {
            let mut directories = VecDeque::from([scope_root.clone()]);
            // The opened listing already is the scope directory's own
            // listing whenever the request named it directly; otherwise the
            // walk starts fresh at the scope root.
            let mut opened = if relative == scope_root { Some(listing) } else { None };
            // Cumulative admitted-entry bound: directories and files count
            // together, so neither the queue nor the listing work can grow
            // without bound. The queue itself shrinks on every pop and is
            // not a cap.
            let mut admitted = 0usize;
            while let Some(directory) = directories.pop_front() {
                let list = match opened.take() {
                    Some(listing) => listing,
                    None => &file_explorer::list_with_generation(generation, root, &directory)
                        .with_context(|| format!("dependency directory unavailable: {directory}"))?,
                };
                coverage.truncated |= list.truncated;
                for entry in &list.entries {
                    if admitted >= MAX_SCAN_FILES {
                        coverage.truncated = true;
                        break;
                    }
                    admitted += 1;
                    if entry.kind == "dir" {
                        // Enqueued paths only ever come from listings of
                        // in-scope directories, so the walk cannot leave the
                        // scope. Once the 80-source budget is full, further
                        // directories are pending work, not new recursion.
                        if sources.len() < MAX_DIRECTORY_FILES {
                            directories.push_back(entry.path.clone());
                        } else {
                            coverage.truncated = true;
                        }
                        continue;
                    }
                    admit_entry(generation, root, entry, &mut sources, &mut coverage)?;
                }
                if (sources.len() >= MAX_DIRECTORY_FILES && !directories.is_empty())
                    || admitted >= MAX_SCAN_FILES
                {
                    // The budget is exhausted and supported sources were met
                    // or are still pending: report truncation and stop the
                    // recursion instead of walking the remainder.
                    coverage.truncated = true;
                    break;
                }
            }
        }
        None => {
            coverage.truncated |= listing.truncated;
            for entry in &listing.entries {
                admit_entry(generation, root, entry, &mut sources, &mut coverage)?;
            }
        }
    }
    let mut caches = ResolutionCaches::default();
    let mut response = assemble_directory(
        path,
        root,
        &sources,
        coverage,
        foundry.as_ref(),
        &|candidate| probe_candidate(generation, root, candidate),
        &mut caches,
        generation,
    )?;
    response.nodes.sort_by(|left, right| left.path.cmp(&right.path));
    if serde_json::to_vec(&response)?.len() > 256 * 1024 {
        return Err(anyhow!("dependency map exceeds response limit"));
    }
    Ok(response)
}

/// Admit one listed entry to the directory scope: unsupported languages are
/// counted and skipped, the 80-source budget truncates explicitly, and
/// vanishable entries (ignored omissions) are counted instead of aborting.
fn admit_entry(
    generation: &ApplicationGeneration,
    root: &Path,
    entry: &file_explorer::ListEntry,
    sources: &mut BTreeMap<String, SourceDependencies>,
    coverage: &mut MapCoverage,
) -> Result<()> {
    if entry.kind != "file" {
        return Ok(());
    }
    if extract_dependencies(&entry.path, "").language == MapLanguage::Unsupported {
        coverage.unsupported_files += 1;
        return Ok(());
    }
    if sources.len() >= MAX_DIRECTORY_FILES {
        coverage.truncated = true;
        return Ok(());
    }
    match read_source(generation, root, &entry.path) {
        Ok(selected) => {
            record_source(coverage, &selected.source);
            sources.insert(selected.relative, selected.source);
        }
        Err(error) if allowed_source_omission(&error) => coverage.unreadable_files += 1,
        Err(error) => return Err(error),
    }
    Ok(())
}

/// One bounded read of one source through a captured generation: the
/// canonical relative identity, the raw bytes of that single read, and the
/// extracted dependencies. Callers that only build the graph move `.source`
/// and drop the bytes; the Ask path paginates those same bytes as the file
/// body, so body and edges share one read.
pub(crate) struct SelectedSource {
    pub relative: String,
    pub bytes: Vec<u8>,
    pub source: SourceDependencies,
}

pub(crate) fn read_source(
    generation: &ApplicationGeneration,
    root: &Path,
    path: &str,
) -> Result<SelectedSource> {
    let (absolute, relative) = file_explorer::resolve_scoped_path(root, path, false)?;
    let bytes = generation.read_file_bounded(&absolute, MAX_SOURCE_BYTES as u64)
        .with_context(|| format!("dependency source unavailable: {relative}"))?;
    let text = std::str::from_utf8(&bytes)
        .with_context(|| format!("dependency source is not UTF-8: {relative}"))?;
    let source = extract_dependencies(&relative, text);
    Ok(SelectedSource { relative, bytes, source })
}

fn record_source(coverage: &mut MapCoverage, source: &SourceDependencies) {
    coverage.scanned_files += 1;
    coverage.unsupported_files += usize::from(source.parse == ParseStatus::Unsupported);
    coverage.incomplete_files += usize::from(source.parse == ParseStatus::Incomplete);
    coverage.dynamic_omitted += source.dynamic_omitted;
}

/// File-map graph under one captured generation: the selected source's
/// outgoing edges, every scanned source's incoming edges, and nodes for the
/// sources that survived the bounded scan.
fn assemble(
    path: &str,
    root: &Path,
    sources: &BTreeMap<String, SourceDependencies>,
    foundry: Option<&FoundryDefault>,
    coverage: MapCoverage,
    probe: &impl Fn(&str) -> Result<CandidateProbe>,
    caches: &mut ResolutionCaches,
    generation: &ApplicationGeneration,
) -> Result<DependencyMapResponse> {
    let mut outgoing = Vec::new();
    let mut incoming = Vec::new();
    let mut nodes = BTreeSet::from([path.to_owned()]);
    for (source_path, source) in sources {
        for evidence in &source.edges {
            let target = resolve_edge(source_path, source, root, evidence, Some(sources), foundry, probe, caches, generation)?;
            let is_incoming = matches!(&target, MapTarget::File { path: target_path } if target_path == path);
            if source_path != path && !is_incoming { continue; }
            nodes.insert(source_path.clone());
            if let MapTarget::File { path } = &target { nodes.insert(path.clone()); }
            let edge = MapEdge { source: source_path.clone(), line: evidence.line, specifier: evidence.specifier.clone(), kind: evidence.kind, target };
            if source_path == path { outgoing.push(edge.clone()); }
            if is_incoming { incoming.push(edge); }
        }
    }
    let nodes = nodes.into_iter().filter_map(|path| {
        sources.get(&path).map(|source| MapNode {
            path,
            language: source.language,
            parse: source.parse,
            purpose: source.declarations.iter().filter_map(|declaration| declaration.doc.as_ref().map(|text| SourcePurpose { line: declaration.line, text: text.clone() })).collect(),
            declarations: source.declarations.clone(),
        })
    }).collect();
    Ok(DependencyMapResponse { path: path.to_owned(), nodes, outgoing, incoming, coverage })
}

/// One-hop file graph for the Ask path: exactly the selected source, its
/// classified outgoing edges, and no incoming edges. Nodes stay the selected
/// file only; existing targets are `File` without any scan-set membership
/// requirement.
fn assemble_one_hop(
    path: &str,
    root: &Path,
    source: &SourceDependencies,
    foundry: Option<&FoundryDefault>,
    coverage: MapCoverage,
    probe: &impl Fn(&str) -> Result<CandidateProbe>,
    caches: &mut ResolutionCaches,
    generation: &ApplicationGeneration,
) -> Result<DependencyMapResponse> {
    let mut outgoing = Vec::new();
    for evidence in &source.edges {
        let target = resolve_edge(path, source, root, evidence, None, foundry, probe, caches, generation)?;
        outgoing.push(MapEdge {
            source: path.to_owned(),
            line: evidence.line,
            specifier: evidence.specifier.clone(),
            kind: evidence.kind,
            target,
        });
    }
    let node = MapNode {
        path: path.to_owned(),
        language: source.language,
        parse: source.parse,
        purpose: source.declarations.iter().filter_map(|declaration| {
            declaration.doc.as_ref().map(|text| SourcePurpose {
                line: declaration.line,
                text: text.clone(),
            })
        }).collect(),
        declarations: source.declarations.clone(),
    };
    Ok(DependencyMapResponse {
        path: path.to_owned(),
        nodes: vec![node],
        outgoing,
        incoming: Vec::new(),
        coverage,
    })
}

/// Directory graph assembly. Every listed source is a node; every edge keeps
/// its classification — `External`, `Unresolved`, and in-workspace `File`
/// targets outside the scope stay edges, and only in-scope `File` targets
/// become nodes.
fn assemble_directory(
    path: &str,
    root: &Path,
    sources: &BTreeMap<String, SourceDependencies>,
    coverage: MapCoverage,
    foundry: Option<&FoundryDefault>,
    probe: &impl Fn(&str) -> Result<CandidateProbe>,
    caches: &mut ResolutionCaches,
    generation: &ApplicationGeneration,
) -> Result<DependencyMapResponse> {
    let mut outgoing = Vec::new();
    let mut nodes = BTreeSet::new();
    for (source_path, source) in sources {
        nodes.insert(source_path.clone());
        for evidence in &source.edges {
            let target = resolve_edge(source_path, source, root, evidence, None, foundry, probe, caches, generation)?;
            if let MapTarget::File { path: target_path } = &target {
                if sources.contains_key(target_path) {
                    nodes.insert(target_path.clone());
                }
            }
            outgoing.push(MapEdge {
                source: source_path.clone(),
                line: evidence.line,
                specifier: evidence.specifier.clone(),
                kind: evidence.kind,
                target,
            });
        }
    }
    let nodes = nodes.into_iter().filter_map(|path| {
        sources.get(&path).map(|source| MapNode {
            path,
            language: source.language,
            parse: source.parse,
            purpose: source.declarations.iter().filter_map(|declaration| {
                declaration.doc.as_ref().map(|text| SourcePurpose {
                    line: declaration.line,
                    text: text.clone(),
                })
            }).collect(),
            declarations: source.declarations.clone(),
        })
    }).collect();
    Ok(DependencyMapResponse {
        path: path.to_owned(),
        nodes,
        outgoing,
        incoming: Vec::new(),
        coverage,
    })
}

fn unresolved(reason: &str) -> MapTarget {
    MapTarget::Unresolved { reason: reason.to_owned() }
}

/// Existence of one relative candidate. `SymlinkBlocked` is only the
/// no-follow `ELOOP` from a strict open; every other failure stays `Err`.
enum CandidateProbe {
    Exists,
    Missing,
    SymlinkBlocked,
}

/// Platform `ELOOP`: `openat2(NO_SYMLINKS)` reports a blocked symlink as 40
/// on Linux and 62 on macOS. `ErrorKind::FilesystemLoop` is unstable here.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn is_symlink_eloop(error: &FsError) -> bool {
    #[cfg(target_os = "macos")]
    const SYMLINK_LOOP_ERRNO: i32 = 62;
    #[cfg(target_os = "linux")]
    const SYMLINK_LOOP_ERRNO: i32 = 40;
    matches!(error, FsError::System { raw_os_error: Some(SYMLINK_LOOP_ERRNO), .. })
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn is_symlink_eloop(_error: &FsError) -> bool {
    false
}

// ---------------------------------------------------------------------------
// TypeScript project aliases (tsconfig paths + baseUrl)
// ---------------------------------------------------------------------------

/// Nearest-tsconfig discovery for one source directory.
#[derive(Debug, Clone)]
enum TsDiscovery {
    Absent,
    /// The workspace-relative tsconfig.json file.
    Config(String),
}

/// Parsed tsconfig extends chain for one config file.
#[derive(Debug, Clone)]
enum TsConfigStatus {
    /// Read successfully but unusable; non-relative edges stay unresolved.
    Invalid(String),
    Valid(TsResolution),
}

/// Merged `compilerOptions` resolution with each option's declaring-config
/// origin: a child silent on `baseUrl` inherits the declaring parent's value,
/// while a child `paths` replaces the parent's wholesale.
#[derive(Debug, Clone)]
struct TsResolution {
    /// `(raw value, declaring config directory)`.
    base_url: Option<(String, String)>,
    /// `(alias → ordered candidates, declaring config directory)`.
    paths: Option<(BTreeMap<String, Vec<String>>, String)>,
}

/// Directory part of one config file path.
fn config_dir_of(file: &str) -> String {
    parent_dir(file).to_string()
}

/// Nearest `tsconfig.json` above one source directory, within the workspace.
fn ts_discovery(
    source_dir: &str,
    probe: &impl Fn(&str) -> Result<CandidateProbe>,
    caches: &mut ResolutionCaches,
) -> Result<TsDiscovery> {
    if let Some(hit) = caches.ts_discovery.get(source_dir) {
        return Ok(hit.clone());
    }
    let mut dir = source_dir.to_string();
    let discovery = loop {
        let candidate = join_relative(&dir, "tsconfig.json");
        match probe(&candidate)? {
            CandidateProbe::Exists => break TsDiscovery::Config(candidate),
            CandidateProbe::Missing | CandidateProbe::SymlinkBlocked => {}
        }
        match dir.rsplit_once('/') {
            Some((parent, _)) => dir = parent.to_string(),
            None if dir.is_empty() => break TsDiscovery::Absent,
            None => dir = String::new(),
        }
    };
    caches.ts_discovery.insert(source_dir.to_string(), discovery.clone());
    Ok(discovery)
}

/// Loads and caches one tsconfig extends chain.
fn ts_config_status(
    config_rel: &str,
    generation: &ApplicationGeneration,
    root: &Path,
    caches: &mut ResolutionCaches,
) -> Result<TsConfigStatus> {
    if let Some(hit) = caches.ts_configs.get(config_rel) {
        return Ok(hit.clone());
    }
    let status = ts_load_config(
        config_rel,
        generation,
        root,
        &mut BTreeSet::from([config_rel.to_string()]),
        0,
    )?;
    caches.ts_configs.insert(config_rel.to_string(), status.clone());
    Ok(status)
}

/// Reads one tsconfig and inherits its `extends` chain. Relative extends
/// only, at most 8 configs; missing, cyclic, oversized, unsupported, and
/// parse failures are explicit `Invalid` values — never silently discarded —
/// while IO and UTF-8 failures propagate.
fn ts_load_config(
    config_rel: &str,
    generation: &ApplicationGeneration,
    root: &Path,
    visited: &mut BTreeSet<String>,
    depth: usize,
) -> Result<TsConfigStatus> {
    if depth >= 8 {
        return Ok(TsConfigStatus::Invalid("the extends chain exceeds 8 configs".into()));
    }
    let (absolute, _) = match file_explorer::resolve_scoped_path(root, config_rel, false) {
        Ok(pair) => pair,
        Err(error) => return Err(error.context(format!("tsconfig {config_rel} unavailable"))),
    };
    let bytes = match generation.read_file_bounded(&absolute, MAX_SOURCE_BYTES as u64) {
        Ok(bytes) => bytes,
        Err(error) => {
            return match error.downcast_ref::<FsError>() {
                Some(FsError::System { kind: std::io::ErrorKind::NotFound, .. }) => {
                    Ok(TsConfigStatus::Invalid("tsconfig.json is gone".into()))
                }
                _ => Err(error.context(format!("tsconfig {config_rel} unavailable"))),
            };
        }
    };
    let text = std::str::from_utf8(&bytes)
        .with_context(|| format!("tsconfig {config_rel} is not UTF-8"))?;
    let stripped = match strip_json_comments_padded(text) {
        Ok(stripped) => stripped,
        Err(_) => return Ok(TsConfigStatus::Invalid("a block comment is not closed".into())),
    };
    let stripped = strip_json_trailing_commas(&stripped);
    let value: serde_json::Value = match serde_json::from_str(&stripped) {
        Ok(value) => value,
        Err(_) => return Ok(TsConfigStatus::Invalid("tsconfig.json is not valid JSON".into())),
    };
    let Some(object) = value.as_object() else {
        return Ok(TsConfigStatus::Invalid("tsconfig.json is not a JSON object".into()));
    };
    let mut resolution = TsResolution { base_url: None, paths: None };
    if let Some(extends) = object.get("extends") {
        let Some(target) = extends.as_str() else {
            return Ok(TsConfigStatus::Invalid("extends is not a string".into()));
        };
        if !(target.starts_with("./") || target.starts_with("../")) {
            return Ok(TsConfigStatus::Invalid("extends target is not a relative path".into()));
        }
        let joined = join_relative(&config_dir_of(config_rel), target);
        let (_, parent_rel) = match file_explorer::resolve_scoped_path(root, &joined, false) {
            Ok(pair) => pair,
            Err(_) => {
                return Ok(TsConfigStatus::Invalid("the extends target escapes the workspace".into()));
            }
        };
        if !visited.insert(parent_rel.clone()) {
            return Ok(TsConfigStatus::Invalid("the extends chain is cyclic".into()));
        }
        resolution = match ts_load_config(&parent_rel, generation, root, visited, depth + 1)? {
            TsConfigStatus::Valid(resolution) => resolution,
            TsConfigStatus::Invalid(reason) => {
                return Ok(TsConfigStatus::Invalid(format!(
                    "the extends target is unusable: {reason}"
                )));
            }
        };
    }
    let Some(options) = object.get("compilerOptions") else {
        return Ok(TsConfigStatus::Valid(resolution));
    };
    let Some(options) = options.as_object() else {
        return Ok(TsConfigStatus::Invalid("compilerOptions is not an object".into()));
    };
    if let Some(base_url) = options.get("baseUrl") {
        let Some(base) = base_url.as_str() else {
            return Ok(TsConfigStatus::Invalid("baseUrl is not a string".into()));
        };
        resolution.base_url = Some((base.to_string(), config_dir_of(config_rel)));
    }
    if let Some(paths) = options.get("paths") {
        let Some(map) = paths.as_object() else {
            return Ok(TsConfigStatus::Invalid("paths is not an object".into()));
        };
        let mut parsed = BTreeMap::new();
        for (key, value) in map {
            let Some(list) = value.as_array() else {
                return Ok(TsConfigStatus::Invalid(format!(
                    "the paths entry {key} is not an array"
                )));
            };
            let mut candidates = Vec::with_capacity(list.len());
            for entry in list {
                let Some(text) = entry.as_str() else {
                    return Ok(TsConfigStatus::Invalid(format!(
                        "the paths entry {key} holds a non-string target"
                    )));
                };
                candidates.push(text.to_string());
            }
            parsed.insert(key.clone(), candidates);
        }
        resolution.paths = Some((parsed, config_dir_of(config_rel)));
    }
    Ok(TsConfigStatus::Valid(resolution))
}

/// Directory alias and baseUrl candidates resolve against: the merged
/// `baseUrl` at its declaring origin when present, otherwise the declaring
/// `paths` config directory. `None` when that base escapes the workspace.
fn ts_candidate_base(resolution: &TsResolution, paths_origin: &str) -> Option<String> {
    match &resolution.base_url {
        Some((base, base_origin)) => join_config_path(base_origin, base),
        None => Some(paths_origin.to_string()),
    }
}

/// Bare (non-relative, non-absolute) TS/TSX/JS import resolution: exact alias
/// keys before wildcard patterns, ordered alternatives, then `baseUrl`, then
/// the historical External fallback. An unusable config keeps such edges
/// explicitly unresolved instead of External.
fn resolve_ts_bare(
    specifier: &str,
    source_dir: &str,
    root: &Path,
    probe: &impl Fn(&str) -> Result<CandidateProbe>,
    caches: &mut ResolutionCaches,
    generation: &ApplicationGeneration,
) -> Result<MapTarget> {
    let config_rel = match ts_discovery(source_dir, probe, caches)? {
        TsDiscovery::Absent => return Ok(external(specifier)),
        TsDiscovery::Config(config_rel) => config_rel,
    };
    let resolution = match ts_config_status(&config_rel, generation, root, caches)? {
        TsConfigStatus::Valid(resolution) => resolution,
        TsConfigStatus::Invalid(reason) => {
            return Ok(unresolved(&format!("tsconfig is unusable: {reason}")));
        }
    };
    if let Some((map, origin)) = &resolution.paths {
        let base_dir = ts_candidate_base(&resolution, origin);
        if let Some(list) = map.get(specifier) {
            return resolve_ts_candidates(list, None, base_dir.as_deref(), root, probe);
        }
        let mut best: Option<(&String, &Vec<String>, String)> = None;
        for (key, list) in map {
            let Some((prefix, suffix)) = key.split_once('*') else {
                continue;
            };
            if specifier.len() < prefix.len() + suffix.len()
                || !specifier.starts_with(prefix)
                || !specifier.ends_with(suffix)
            {
                continue;
            }
            let middle = specifier[prefix.len()..specifier.len() - suffix.len()].to_string();
            let longer = match &best {
                None => true,
                Some((current, _, _)) => {
                    prefix.len() > current.split_once('*').expect("checked").0.len()
                }
            };
            if longer {
                best = Some((key, list, middle));
            }
        }
        if let Some((_, list, wildcard)) = best {
            return resolve_ts_candidates(list, Some(&wildcard), base_dir.as_deref(), root, probe);
        }
    }
    if let Some((base, origin)) = &resolution.base_url {
        // A baseUrl (or specifier join) escaping the workspace is an explicit
        // failure, never the External fallback.
        let Some(joined) =
            join_config_path(origin, base).and_then(|base_dir| join_config_path(&base_dir, specifier))
        else {
            return Ok(unresolved("the tsconfig baseUrl escapes the workspace"));
        };
        // Any other scope rejection (for example a NUL smuggled through the
        // config value) is an explicit failure, never the External fallback.
        let (_, candidate) = match file_explorer::resolve_scoped_path(root, &joined, false) {
            Ok(pair) => pair,
            Err(_) => {
                return Ok(unresolved(
                    "the tsconfig baseUrl candidate is not a workspace-relative path",
                ));
            }
        };
        match classify_candidates(relative_candidates(&candidate, false), probe)? {
            CandidateOutcome::File(path) => return Ok(MapTarget::File { path }),
            CandidateOutcome::Missing { symlink: true } => {
                return Ok(unresolved("symlink target is not followed"));
            }
            CandidateOutcome::Missing { symlink: false } => return Ok(external(specifier)),
            CandidateOutcome::Multiple => {
                return Ok(unresolved("multiple relative file candidates"));
            }
        }
    }
    Ok(external(specifier))
}

/// Probes one alias's ordered target templates; the first resolvable
/// alternative wins, ambiguity inside one alternative stays unresolved, and a
/// fully exhausted alias stays explicitly unresolved.
fn resolve_ts_candidates(
    templates: &[String],
    wildcard: Option<&str>,
    base_dir: Option<&str>,
    root: &Path,
    probe: &impl Fn(&str) -> Result<CandidateProbe>,
) -> Result<MapTarget> {
    // A base that escapes the workspace leaves no usable alias candidate.
    let Some(base_dir) = base_dir else {
        return Ok(unresolved("the tsconfig baseUrl escapes the workspace"));
    };
    for template in templates {
        let target = match wildcard {
            Some(wildcard) => match template.split_once('*') {
                Some((prefix, suffix)) => format!("{prefix}{wildcard}{suffix}"),
                None => template.clone(),
            },
            None => template.clone(),
        };
        let joined = match join_config_path(base_dir, &target) {
            Some(joined) => joined,
            // A template that escapes the workspace is not a resolvable
            // alternative; the next one is tried.
            None => continue,
        };
        let Ok((_, base)) = file_explorer::resolve_scoped_path(root, &joined, false) else {
            continue;
        };
        match classify_candidates(relative_candidates(&base, false), probe)? {
            CandidateOutcome::File(path) => return Ok(MapTarget::File { path }),
            CandidateOutcome::Missing { symlink: true } => {
                return Ok(unresolved("symlink target is not followed"));
            }
            CandidateOutcome::Missing { symlink: false } => continue,
            CandidateOutcome::Multiple => {
                return Ok(unresolved("multiple relative file candidates"));
            }
        }
    }
    Ok(unresolved("no tsconfig paths candidate exists"))
}

/// Extension candidate set for one extension-less base (TS/JS; Solidity
/// probes the exact path only).
fn relative_candidates(base: &str, solidity: bool) -> BTreeSet<String> {
    let mut candidates = BTreeSet::from([base.to_string()]);
    if !solidity && Path::new(base).extension().is_none() {
        for suffix in [".ts", ".tsx", ".js", ".jsx", ".mts", ".cts", ".mjs", ".cjs"] {
            candidates.insert(format!("{base}{suffix}"));
            candidates.insert(format!("{base}/index{suffix}"));
        }
    }
    candidates
}

/// Outcome of probing one candidate set with the shared probe.
enum CandidateOutcome {
    File(String),
    /// No candidate exists; `symlink` records a blocked-link probe.
    Missing { symlink: bool },
    Multiple,
}

fn classify_candidates(
    candidates: BTreeSet<String>,
    probe: &impl Fn(&str) -> Result<CandidateProbe>,
) -> Result<CandidateOutcome> {
    let mut matches = Vec::new();
    let mut saw_symlink = false;
    for candidate in candidates {
        match probe(&candidate)? {
            CandidateProbe::Exists => matches.push(candidate),
            CandidateProbe::Missing => {}
            CandidateProbe::SymlinkBlocked => saw_symlink = true,
        }
    }
    Ok(match matches.as_slice() {
        [path] => CandidateOutcome::File(path.clone()),
        [] => CandidateOutcome::Missing { symlink: saw_symlink },
        _ => CandidateOutcome::Multiple,
    })
}

/// Removes `,` separators outside strings whose next character is `}` or
/// `]`, the trailing-comma tolerance JSONC sources rely on.
fn strip_json_trailing_commas(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut in_string = false;
    let mut escaped = false;
    let mut index = 0;
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
        if current == b',' {
            let mut lookahead = index + 1;
            while matches!(bytes.get(lookahead), Some(b' ' | b'\t' | b'\r' | b'\n')) {
                lookahead += 1;
            }
            if matches!(bytes.get(lookahead), Some(b'}' | b']')) {
                index += 1;
                continue;
            }
        }
        output.push(current);
        index += 1;
    }
    String::from_utf8(output).unwrap_or_else(|_| text.to_owned())
}

// ---------------------------------------------------------------------------
// Go package resolution
// ---------------------------------------------------------------------------

/// Request-scoped resolution caches under one plain `&mut` borrow. Concrete
/// fields only: no generic environment, no interior mutability, no `dyn`
/// indirection. Every entry is derived once per request from the captured
/// generation and reused by every edge of the same map.
#[derive(Default)]
struct ResolutionCaches {
    /// Nearest crate directory → parsed Cargo target verdict.
    cargo_manifests: BTreeMap<String, CargoManifest>,
    /// Source file → proven Cargo module chain.
    rust_chains: BTreeMap<String, RustChain>,
    /// Module/use-path files → extracted dependencies (the selected source is
    /// seeded once so a chain never re-reads it).
    rust_files: BTreeMap<String, SourceDependencies>,
    /// Source directory → nearest tsconfig.json discovery.
    ts_discovery: BTreeMap<String, TsDiscovery>,
    /// tsconfig.json file → parsed extends chain.
    ts_configs: BTreeMap<String, TsConfigStatus>,
    /// Import path → resolved Go target.
    go_probes: BTreeMap<String, MapTarget>,
    /// Package directory → proven package verdict.
    go_packages: BTreeMap<String, GoPackageStatus>,
    /// Non-test Go member files additionally read across every import of
    /// this request, on top of the sources the shared scan already paid
    /// for; one request-wide total budget, never a second cap.
    go_member_reads: usize,
    /// Go package-clause evidence already captured outside the proofs (the
    /// selected source): path → clause; proofs reuse it, never re-reading.
    go_member_clauses: BTreeMap<String, Option<String>>,
    /// The workspace root's go.mod verdict, read once per request.
    go_modules: Option<GoModuleStatus>,
}

/// Cached root go.mod verdict. Only the workspace root's go.mod defines the
/// module; nested manifests are outside the frozen contract.
#[derive(Debug, Clone)]
enum GoModuleStatus {
    Absent,
    Invalid(String),
    Valid(String),
}

/// Cached package proof for one directory.
#[derive(Debug, Clone)]
enum GoPackageStatus {
    Missing,
    Unresolved(String),
    Package,
}

fn external(specifier: &str) -> MapTarget {
    MapTarget::External { specifier: specifier.to_string() }
}

/// Directory part of a `/`-separated workspace-relative path.
fn parent_dir(path: &str) -> &str {
    path.rsplit_once('/').map(|(parent, _)| parent).unwrap_or("")
}

/// Joins a workspace-relative directory with one more segment.
fn join_relative(dir: &str, segment: &str) -> String {
    if dir.is_empty() {
        segment.to_string()
    } else {
        format!("{dir}/{segment}")
    }
}

/// Joins a config-relative path onto a workspace-relative directory,
/// skipping `.` segments and applying `..` lexically. `None` means the join
/// cannot name a workspace-relative path at all — a `..` that underflows the
/// workspace root or an absolute path — so the caller must never probe a
/// silently flattened look-alike.
fn join_config_path(dir: &str, path: &str) -> Option<String> {
    if path.starts_with('/') {
        return None;
    }
    let mut joined = dir.to_string();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                if joined.is_empty() {
                    return None;
                }
                joined = parent_dir(&joined).to_string();
            }
            other => joined = join_relative(&joined, other),
        }
    }
    Some(joined)
}

/// A canonical Go import/package path: POSIX segments, none empty, none `.`
/// or `..`, no backslash, no leading slash.
fn go_canonical_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains('\\')
        && path.split('/').all(|segment| !segment.is_empty() && segment != "." && segment != "..")
}

/// Conservatively recognized standard-library first segments plus the cgo
/// pseudo-package. Anything else is unprovable without a go.mod.
fn is_go_stdlib_or_cgo(import: &str) -> bool {
    if import == "C" {
        return true;
    }
    matches!(
        import.split('/').next().unwrap_or(""),
        "archive" | "bufio" | "bytes" | "cmp" | "compress" | "container"
            | "context" | "crypto" | "database" | "debug" | "embed" | "encoding"
            | "errors" | "expvar" | "flag" | "fmt" | "go" | "hash" | "html" | "image"
            | "index" | "io" | "log" | "maps" | "math" | "mime" | "net" | "os"
            | "path" | "plugin" | "reflect" | "regexp" | "runtime" | "slices" | "sort"
            | "strconv" | "strings" | "structs" | "sync" | "syscall" | "testing"
            | "text" | "time" | "unicode" | "unique" | "unsafe" | "weak"
    )
}

/// The `package` clause of one Go source file, or `None` when the leading
/// tokens do not form one. Comments and whitespace are skipped; the keyword
/// must be a whole word followed by whitespace and an identifier.
fn go_package_clause(source: &str) -> Option<String> {
    let mut chars = source.char_indices().peekable();
    loop {
        while matches!(chars.peek(), Some(&(_, ch)) if ch.is_whitespace()) {
            chars.next();
        }
        match chars.peek().map(|&(_, ch)| ch) {
            Some('/') => {
                chars.next();
                match chars.next().map(|(_, ch)| ch) {
                    Some('/') => {
                        while let Some((_, ch)) = chars.next() {
                            if ch == '\n' {
                                break;
                            }
                        }
                    }
                    Some('*') => loop {
                        match chars.next() {
                            None => return None,
                            Some((_, '*')) => {
                                if chars.peek().map(|&(_, ch)| ch) == Some('/') {
                                    chars.next();
                                    break;
                                }
                            }
                            Some(_) => {}
                        }
                    },
                    _ => return None,
                }
            }
            Some(_) => break,
            None => return None,
        }
    }
    let mut keyword = String::new();
    while matches!(chars.peek(), Some(&(_, ch)) if ch.is_ascii_alphabetic() || ch == '_') {
        keyword.push(chars.next().expect("peeked").1);
    }
    if keyword != "package" {
        return None;
    }
    match chars.peek() {
        Some((_, ch)) if ch.is_whitespace() => {}
        _ => return None,
    }
    while matches!(chars.peek(), Some(&(_, ch)) if ch.is_whitespace()) {
        chars.next();
    }
    let mut name = String::new();
    while matches!(chars.peek(), Some(&(_, ch)) if ch.is_ascii_alphanumeric() || ch == '_') {
        name.push(chars.next().expect("peeked").1);
    }
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

/// Cuts one go.mod line at its first unquoted `//` comment.
fn cut_go_mod_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut in_string = false;
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'"' => in_string = !in_string,
            b'/' if !in_string && bytes.get(index + 1) == Some(&b'/') => return &line[..index],
            _ => {}
        }
        index += 1;
    }
    line
}

/// Parses the `module` directive out of a successfully read go.mod. Invalid
/// syntax or shape is an explicit `Invalid`, never a guess.
fn parse_go_mod(text: &str) -> GoModuleStatus {
    let mut module: Option<String> = None;
    for line in text.lines() {
        let line = cut_go_mod_comment(line).trim();
        if line.is_empty() {
            continue;
        }
        let Some(rest) = line.strip_prefix("module") else {
            continue;
        };
        if !rest.starts_with([' ', '\t']) {
            continue;
        }
        if module.is_some() {
            return GoModuleStatus::Invalid("go.mod declares module twice".into());
        }
        let path = rest.trim();
        let path = match (path.starts_with('"'), path.ends_with('"')) {
            (true, true) if path.len() >= 2 => &path[1..path.len() - 1],
            (false, false) => path,
            _ => return GoModuleStatus::Invalid("the module path is not a plain or quoted token".into()),
        };
        if !go_canonical_path(path) {
            return GoModuleStatus::Invalid("the module path is not a canonical package path".into());
        }
        module = Some(path.to_string());
    }
    match module {
        Some(module) => GoModuleStatus::Valid(module),
        None => GoModuleStatus::Invalid("go.mod declares no module".into()),
    }
}

/// The workspace root's go.mod verdict, read once per request.
fn go_module_status(
    root: &Path,
    caches: &mut ResolutionCaches,
    generation: &ApplicationGeneration,
) -> Result<GoModuleStatus> {
    if let Some(hit) = caches.go_modules.as_ref() {
        return Ok(hit.clone());
    }
    let (absolute, _) = match file_explorer::resolve_scoped_path(root, "go.mod", false) {
        Ok(pair) => pair,
        Err(error) => return Err(error.context("go.mod unavailable")),
    };
    let status = match generation.read_file_bounded(&absolute, MAX_SOURCE_BYTES as u64) {
        Ok(bytes) => {
            let text = std::str::from_utf8(&bytes).with_context(|| "go.mod is not UTF-8")?;
            parse_go_mod(text)
        }
        Err(error) => match error.downcast_ref::<FsError>() {
            Some(FsError::System { kind: std::io::ErrorKind::NotFound, .. }) => GoModuleStatus::Absent,
            _ => return Err(error.context("go.mod unavailable")),
        },
    };
    caches.go_modules = Some(status.clone());
    Ok(status)
}

/// Proves one directory is a Go package: an untruncated listing whose visible
/// non-`_test.go` members all parse a same-named `package` clause.
fn go_package_status(
    directory: &str,
    root: &Path,
    scanned: Option<&BTreeMap<String, SourceDependencies>>,
    caches: &mut ResolutionCaches,
    generation: &ApplicationGeneration,
) -> Result<GoPackageStatus> {
    if let Some(hit) = caches.go_packages.get(directory) {
        return Ok(hit.clone());
    }
    let listing_path = if directory == "." { "" } else { directory };
    let listing = match file_explorer::list_with_generation(generation, root, listing_path) {
        Ok(listing) => listing,
        Err(error) => {
            return match error.downcast_ref::<FileExplorerError>() {
                Some(FileExplorerError::DirectoryUnavailable) => Ok(GoPackageStatus::Missing),
                _ => Err(error),
            };
        }
    };
    let status = if listing.truncated {
        GoPackageStatus::Unresolved(
            "the package directory listing was truncated; membership is not provable".into(),
        )
    } else {
        go_package_proof(&listing, root, scanned, caches, generation)?
    };
    caches.go_packages.insert(directory.to_string(), status.clone());
    Ok(status)
}

fn go_package_proof(
    listing: &file_explorer::ListResponse,
    root: &Path,
    scanned: Option<&BTreeMap<String, SourceDependencies>>,
    caches: &mut ResolutionCaches,
    generation: &ApplicationGeneration,
) -> Result<GoPackageStatus> {
    let members: Vec<&file_explorer::ListEntry> = listing
        .entries
        .iter()
        .filter(|entry| {
            entry.kind == "file" && entry.name.ends_with(".go") && !entry.name.ends_with("_test.go")
        })
        .collect();
    if members.is_empty() {
        return Ok(GoPackageStatus::Unresolved(
            "the package directory contains no non-test Go source".into(),
        ));
    }
    let mut package: Option<String> = None;
    for member in members {
        let name = match caches.go_member_clauses.get(&member.path) {
            // Captured this request: reuse the evidence, never re-read.
            Some(cached) => cached.clone(),
            None => {
                // The counter is seeded by the sources the request already
                // read and counts each unique path once: a member the scan
                // already paid for is read again but not charged twice.
                let already_scanned = scanned.is_some_and(|map| map.contains_key(&member.path));
                if !already_scanned {
                    let spent = caches.go_member_reads + scanned.map_or(0, BTreeMap::len);
                    if spent >= MAX_SCAN_FILES {
                        return Ok(GoPackageStatus::Unresolved(
                            "the shared package-proof read budget is exhausted".into(),
                        ));
                    }
                    caches.go_member_reads += 1;
                }
                let (absolute, _) =
                    match file_explorer::resolve_scoped_path(root, &member.path, false) {
                        Ok(pair) => pair,
                        Err(error) => {
                            return Err(error
                                .context(format!("go package member {} unavailable", member.path)));
                        }
                    };
                let bytes = match generation.read_file_bounded(&absolute, MAX_SOURCE_BYTES as u64) {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        if allowed_source_omission(&error) {
                            // A visible member that cannot be read leaves
                            // the member set incomplete: not provable.
                            return Ok(GoPackageStatus::Unresolved(format!(
                                "go package member {} could not be read; membership is not provable",
                                member.path
                            )));
                        }
                        return Err(error
                            .context(format!("go package member {} unavailable", member.path)));
                    }
                };
                let text = std::str::from_utf8(&bytes)
                    .with_context(|| format!("go package member {} is not UTF-8", member.path))?;
                let name = go_package_clause(text);
                caches.go_member_clauses.insert(member.path.clone(), name.clone());
                name
            }
        };
        let Some(name) = name else {
            return Ok(GoPackageStatus::Unresolved(format!(
                "go package member {} declares no package clause",
                member.path
            )));
        };
        match &package {
            None => package = Some(name),
            Some(current) if *current == name => {}
            Some(_) => {
                return Ok(GoPackageStatus::Unresolved(
                    "go package members disagree on the package name".into(),
                ));
            }
        }
    }
    Ok(GoPackageStatus::Package)
}

/// Maps one Go import to its package: the module prefix with an exact slash
/// boundary names local directories (the module root is `".`"), everything
/// else is an external module. A matching directory that is absent, empty,
/// invalid, or listed only truncately stays explicitly unresolved — never
/// External.
fn resolve_go_import(
    specifier: &str,
    root: &Path,
    scanned: Option<&BTreeMap<String, SourceDependencies>>,
    caches: &mut ResolutionCaches,
    generation: &ApplicationGeneration,
) -> Result<MapTarget> {
    if let Some(hit) = caches.go_probes.get(specifier) {
        return Ok(hit.clone());
    }
    let target = if !go_canonical_path(specifier) {
        unresolved("the go import path is not a canonical package path")
    } else {
        match go_module_status(root, caches, generation)? {
            GoModuleStatus::Valid(module) => {
                if specifier == module {
                    go_package_target(".", root, scanned, caches, generation)?
                } else if let Some(rest) = specifier.strip_prefix(&format!("{module}/")) {
                    if go_canonical_path(rest) {
                        go_package_target(rest, root, scanned, caches, generation)?
                    } else {
                        unresolved(
                            "the module-relative import does not map to a canonical package directory",
                        )
                    }
                } else {
                    external(specifier)
                }
            }
            GoModuleStatus::Absent => {
                if is_go_stdlib_or_cgo(specifier) {
                    external(specifier)
                } else {
                    unresolved(
                        "no go.mod; the import is neither a recognized standard library nor workspace-local",
                    )
                }
            }
            GoModuleStatus::Invalid(reason) => unresolved(&format!("go.mod is unusable: {reason}")),
        }
    };
    caches.go_probes.insert(specifier.to_string(), target.clone());
    Ok(target)
}

/// Classifies one module-mapped directory as a proven Go package target.
fn go_package_target(
    directory: &str,
    root: &Path,
    scanned: Option<&BTreeMap<String, SourceDependencies>>,
    caches: &mut ResolutionCaches,
    generation: &ApplicationGeneration,
) -> Result<MapTarget> {
    Ok(match go_package_status(directory, root, scanned, caches, generation)? {
        GoPackageStatus::Package => MapTarget::Package { dir: directory.to_string() },
        GoPackageStatus::Missing => unresolved(&format!(
            "the module package directory {directory} does not exist"
        )),
        GoPackageStatus::Unresolved(reason) => unresolved(&reason),
    })
}

// ---------------------------------------------------------------------------
// Rust module chains and local use paths
// ---------------------------------------------------------------------------

/// Parsed Cargo target verdict for one crate directory.
#[derive(Debug, Clone)]
enum CargoManifest {
    /// Every target root the manifest declares or auto-discovers.
    Trusted(CargoTargets),
    /// Read successfully but unusable: invalid TOML or an unprovable target
    /// declaration. Relevant edges stay explicitly unresolved.
    Invalid(String),
}

#[derive(Debug, Clone)]
struct CargoTargets {
    roots: Vec<CargoTargetRoot>,
    /// Auto target directories whose listing was truncated; sources beneath
    /// them stay unprovable.
    truncated_dirs: Vec<String>,
}

/// One Cargo target root: the file that roots a module tree and the directory
/// its child modules live in.
#[derive(Debug, Clone)]
struct CargoTargetRoot {
    file: String,
    children_dir: String,
}

/// Proof that one source file belongs to a declared Cargo target module tree.
#[derive(Debug, Clone)]
enum RustChain {
    Proven(ProvenChain),
    Unproven(String),
}

#[derive(Debug, Clone)]
struct ProvenChain {
    /// Directory of the crate root's child modules (the `crate::` scope).
    crate_dir: String,
    /// The crate root file, the target of a zero-depth `crate::` prefix.
    root_file: String,
    /// Directory the source's own child modules live in.
    children_dir: String,
    /// File-backed module path from the crate root, outermost first.
    module_path: Vec<String>,
    /// Files from the crate root to the source; `chain[i]` is the file of the
    /// first `i` path modules, so `super::` can walk back.
    chain_files: Vec<String>,
}

enum RustWalk {
    Proven(ProvenChain),
    Conservative(String),
    Broken(String),
}

/// Module file stem: raw identifiers map to the plain file name.
fn module_file_stem(name: &str) -> &str {
    name.strip_prefix("r#").unwrap_or(name)
}

/// Directory a non-root module file's child modules live in: `foo.rs` spans
/// `foo/`, `foo/mod.rs` spans its own directory.
fn module_children_dir(file: &str) -> String {
    let (dir, base) = match file.rsplit_once('/') {
        Some((parent, base)) => (parent, base),
        None => ("", file),
    };
    if base == "mod.rs" {
        dir.to_string()
    } else {
        let stem = base.strip_suffix(".rs").unwrap_or(base);
        join_relative(dir, stem)
    }
}

/// Directory prefix for one inline lineage below a base directory.
fn lineage_dir(base: &str, lineage: &[String]) -> String {
    let mut dir = base.to_string();
    for segment in lineage {
        dir = join_relative(&dir, segment);
    }
    dir
}

/// An explicit target path must be a relative in-crate POSIX path to take the
/// module convention.
fn cargo_target_path_usable(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains('\\')
        && !path.split('/').any(|segment| segment.is_empty() || segment == "." || segment == "..")
}

fn push_unique_root(roots: &mut Vec<CargoTargetRoot>, file: String) {
    if !roots.iter().any(|existing| existing.file == file) {
        let children_dir = parent_dir(&file).to_string();
        roots.push(CargoTargetRoot { file, children_dir });
    }
}

/// Routes one explicitly declared target entry to its conventional file:
/// `<dir>/<name>.rs` or `<dir>/<name>/main.rs`, never both. `Ok(Some)`
/// carries the manifest-invalid reason for a target declared in both forms.
fn push_named_target(
    roots: &mut Vec<CargoTargetRoot>,
    probe: &impl Fn(&str) -> Result<CandidateProbe>,
    directory: &str,
    name: &str,
    kind: &str,
) -> Result<Option<String>> {
    let first = join_relative(directory, &format!("{name}.rs"));
    let second = join_relative(directory, &format!("{name}/main.rs"));
    let first_exists = matches!(probe(&first)?, CandidateProbe::Exists);
    let second_exists = matches!(probe(&second)?, CandidateProbe::Exists);
    if first_exists && second_exists {
        return Ok(Some(format!("{kind} target {name} has both module and directory forms")));
    }
    if first_exists {
        push_unique_root(roots, first);
    } else if second_exists {
        push_unique_root(roots, second);
    }
    Ok(None)
}

/// Nearest Cargo.toml above the source, within the workspace root.
fn rust_crate_dir(
    source: &str,
    probe: &impl Fn(&str) -> Result<CandidateProbe>,
) -> Result<Option<String>> {
    let mut dir = parent_dir(source).to_string();
    loop {
        let candidate = join_relative(&dir, "Cargo.toml");
        if matches!(probe(&candidate)?, CandidateProbe::Exists) {
            return Ok(Some(dir));
        }
        match dir.rsplit_once('/') {
            Some((parent, _)) => dir = parent.to_string(),
            None if dir.is_empty() => return Ok(None),
            None => dir = String::new(),
        }
    }
}

/// The crate's Cargo target verdict, read and parsed once per request.
fn cargo_manifest(
    crate_dir: &str,
    root: &Path,
    probe: &impl Fn(&str) -> Result<CandidateProbe>,
    caches: &mut ResolutionCaches,
    generation: &ApplicationGeneration,
) -> Result<CargoManifest> {
    if let Some(hit) = caches.cargo_manifests.get(crate_dir) {
        return Ok(hit.clone());
    }
    let manifest = cargo_manifest_uncached(crate_dir, root, probe, caches, generation)?;
    caches.cargo_manifests.insert(crate_dir.to_string(), manifest.clone());
    Ok(manifest)
}

fn cargo_manifest_uncached(
    crate_dir: &str,
    root: &Path,
    probe: &impl Fn(&str) -> Result<CandidateProbe>,
    caches: &mut ResolutionCaches,
    generation: &ApplicationGeneration,
) -> Result<CargoManifest> {
    let manifest_rel = join_relative(crate_dir, "Cargo.toml");
    let (absolute, _) = match file_explorer::resolve_scoped_path(root, &manifest_rel, false) {
        Ok(pair) => pair,
        Err(error) => return Err(error.context(format!("cargo manifest {manifest_rel} unavailable"))),
    };
    let bytes = match generation.read_file_bounded(&absolute, MAX_SOURCE_BYTES as u64) {
        Ok(bytes) => bytes,
        Err(error) => {
            return match error.downcast_ref::<FsError>() {
                Some(FsError::System { kind: std::io::ErrorKind::NotFound, .. }) => {
                    Ok(CargoManifest::Invalid("the manifest vanished".into()))
                }
                _ => Err(error.context(format!("cargo manifest {manifest_rel} unavailable"))),
            };
        }
    };
    let text = std::str::from_utf8(&bytes)
        .with_context(|| format!("cargo manifest {manifest_rel} is not UTF-8"))?;
    let table: toml::Table = match toml::from_str(text) {
        Ok(table) => table,
        Err(_) => return Ok(CargoManifest::Invalid("the manifest is not valid TOML".into())),
    };
    let package = table.get("package").and_then(|value| value.as_table());
    let auto_flag = |key: &str| -> bool {
        package
            .and_then(|package| package.get(key))
            .and_then(|value| value.as_bool())
            .unwrap_or(true)
    };
    let autolib = auto_flag("autolib");
    let autobins = auto_flag("autobins");
    let autotests = auto_flag("autotests");
    let autoexamples = auto_flag("autoexamples");
    let autobenches = auto_flag("autobenches");
    let in_crate = |relative: &str| join_relative(crate_dir, relative);
    let mut roots: Vec<CargoTargetRoot> = Vec::new();
    let mut truncated_dirs: Vec<String> = Vec::new();
    let lib = table.get("lib").and_then(|value| value.as_table());
    match lib.and_then(|lib| lib.get("path")).and_then(|value| value.as_str()) {
        Some(path) if cargo_target_path_usable(path) => push_unique_root(&mut roots, in_crate(path)),
        Some(_) => {}
        None => {
            if autolib && matches!(probe(&in_crate("src/lib.rs"))?, CandidateProbe::Exists) {
                push_unique_root(&mut roots, in_crate("src/lib.rs"));
            }
        }
    }
    if let Some(bins) = table.get("bin").and_then(|value| value.as_array()) {
        for bin in bins.iter().filter_map(|bin| bin.as_table()) {
            match bin.get("path").and_then(|value| value.as_str()) {
                Some(path) if cargo_target_path_usable(path) => {
                    push_unique_root(&mut roots, in_crate(path));
                }
                Some(_) => {}
                None => {
                    let Some(name) = bin.get("name").and_then(|value| value.as_str()) else {
                        continue;
                    };
                    if let Some(reason) =
                        push_named_target(&mut roots, probe, &in_crate("src/bin"), name, "bin")?
                    {
                        return Ok(CargoManifest::Invalid(reason));
                    }
                }
            }
        }
    }
    if autobins {
        cargo_auto_dir(&in_crate("src/bin"), root, probe, generation, &mut roots, &mut truncated_dirs)?;
        if matches!(probe(&in_crate("src/main.rs"))?, CandidateProbe::Exists) {
            push_unique_root(&mut roots, in_crate("src/main.rs"));
        }
    }
    for (kind, auto) in [("test", autotests), ("example", autoexamples), ("bench", autobenches)] {
        let directory = format!("{kind}s");
        if let Some(entries) = table.get(kind).and_then(|value| value.as_array()) {
            for entry in entries.iter().filter_map(|entry| entry.as_table()) {
                match entry.get("path").and_then(|value| value.as_str()) {
                    Some(path) if cargo_target_path_usable(path) => {
                        push_unique_root(&mut roots, in_crate(path));
                    }
                    Some(_) => {}
                    None => {
                        let Some(name) = entry.get("name").and_then(|value| value.as_str()) else {
                            continue;
                        };
                        if let Some(reason) =
                            push_named_target(&mut roots, probe, &in_crate(&directory), name, kind)?
                        {
                            return Ok(CargoManifest::Invalid(reason));
                        }
                    }
                }
            }
        }
        if auto {
            cargo_auto_dir(&in_crate(&directory), root, probe, generation, &mut roots, &mut truncated_dirs)?;
        }
    }
    Ok(CargoManifest::Trusted(CargoTargets { roots, truncated_dirs }))
}

/// Auto target roots under one directory: top-level `.rs` files root their own
/// targets and `<dir>/<name>/main.rs` roots a directory target. A truncated
/// listing is recorded — never guessed around — and a missing directory
/// simply contributes no roots.
fn cargo_auto_dir(
    directory: &str,
    root: &Path,
    probe: &impl Fn(&str) -> Result<CandidateProbe>,
    generation: &ApplicationGeneration,
    roots: &mut Vec<CargoTargetRoot>,
    truncated_dirs: &mut Vec<String>,
) -> Result<()> {
    let listing = match file_explorer::list_with_generation(generation, root, directory) {
        Ok(listing) => listing,
        Err(error) => {
            return match error.downcast_ref::<FileExplorerError>() {
                Some(FileExplorerError::DirectoryUnavailable) => Ok(()),
                _ => Err(error),
            };
        }
    };
    if listing.truncated {
        truncated_dirs.push(directory.to_string());
        return Ok(());
    }
    for entry in &listing.entries {
        match entry.kind {
            "file" if entry.name.ends_with(".rs") => push_unique_root(roots, entry.path.clone()),
            "dir" => {
                let main = join_relative(&entry.path, "main.rs");
                if matches!(probe(&main)?, CandidateProbe::Exists) {
                    push_unique_root(roots, main);
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Whether one target root's module tree can even contain the source.
fn rust_root_applies(source: &str, target: &CargoTargetRoot) -> bool {
    if source == target.file {
        return true;
    }
    if !source.starts_with(&join_relative(&target.children_dir, "")) {
        return false;
    }
    // The canonical lib/main trees never reach into src/bin: those files are
    // their own bin targets.
    let canonical_root = target.file == join_relative(&target.children_dir, "lib.rs")
        || target.file == join_relative(&target.children_dir, "main.rs");
    !(canonical_root && source.starts_with(&join_relative(&target.children_dir, "bin/")))
}

/// The proven module chain of one source file, cached per request.
fn rust_chain(
    source: &str,
    root: &Path,
    scanned: Option<&BTreeMap<String, SourceDependencies>>,
    probe: &impl Fn(&str) -> Result<CandidateProbe>,
    caches: &mut ResolutionCaches,
    generation: &ApplicationGeneration,
) -> Result<RustChain> {
    if let Some(hit) = caches.rust_chains.get(source) {
        return Ok(hit.clone());
    }
    let chain = rust_chain_uncached(source, root, scanned, probe, caches, generation)?;
    caches.rust_chains.insert(source.to_string(), chain.clone());
    Ok(chain)
}

fn rust_chain_uncached(
    source: &str,
    root: &Path,
    scanned: Option<&BTreeMap<String, SourceDependencies>>,
    probe: &impl Fn(&str) -> Result<CandidateProbe>,
    caches: &mut ResolutionCaches,
    generation: &ApplicationGeneration,
) -> Result<RustChain> {
    let Some(crate_dir) = rust_crate_dir(source, probe)? else {
        return Ok(RustChain::Unproven(
            "no Cargo.toml above the source; the module chain root is unprovable".into(),
        ));
    };
    let targets = match cargo_manifest(&crate_dir, root, probe, caches, generation)? {
        CargoManifest::Trusted(targets) => targets,
        CargoManifest::Invalid(reason) => {
            return Ok(RustChain::Unproven(format!("the cargo manifest is unusable: {reason}")));
        }
    };
    for truncated in &targets.truncated_dirs {
        if source.starts_with(&join_relative(truncated, "")) {
            return Ok(RustChain::Unproven(format!(
                "the {truncated} target listing was truncated; module membership is not provable"
            )));
        }
    }
    let mut proven: Option<ProvenChain> = None;
    let mut conservative: Option<String> = None;
    let mut broken: Option<String> = None;
    for target in &targets.roots {
        if !rust_root_applies(source, target) {
            continue;
        }
        match rust_walk_from_root(source, root, target, scanned, probe, caches, generation)? {
            RustWalk::Proven(chain) => {
                if let Some(previous) = &proven {
                    if previous.crate_dir != chain.crate_dir
                        || previous.module_path != chain.module_path
                    {
                        return Ok(RustChain::Unproven(
                            "the source is proven from cargo targets with differing module contexts"
                                .into(),
                        ));
                    }
                } else {
                    proven = Some(chain);
                }
            }
            RustWalk::Conservative(reason) => {
                if conservative.is_none() {
                    conservative = Some(reason);
                }
            }
            RustWalk::Broken(reason) => {
                if broken.is_none() {
                    broken = Some(reason);
                }
            }
        }
    }
    Ok(match proven {
        Some(chain) => RustChain::Proven(chain),
        None => RustChain::Unproven(
            conservative
                .or(broken)
                .unwrap_or_else(|| "no cargo target module chain reaches the source".into()),
        ),
    })
}

/// Walks the declared module chain from one target root to the source.
/// Every step is a [`rust_declared_step`]: inline ancestors descend
/// lexically, file-backed modules route to their unique conventional file,
/// and each routed file must sit on the source's ancestor path. The source
/// itself is never re-read.
fn rust_walk_from_root(
    source: &str,
    root: &Path,
    target: &CargoTargetRoot,
    scanned: Option<&BTreeMap<String, SourceDependencies>>,
    probe: &impl Fn(&str) -> Result<CandidateProbe>,
    caches: &mut ResolutionCaches,
    generation: &ApplicationGeneration,
) -> Result<RustWalk> {
    if source == target.file {
        return Ok(RustWalk::Proven(ProvenChain {
            crate_dir: target.children_dir.clone(),
            root_file: target.file.clone(),
            children_dir: target.children_dir.clone(),
            module_path: Vec::new(),
            chain_files: vec![target.file.clone()],
        }));
    }
    let prefix = join_relative(&target.children_dir, "");
    let Some(relative) = source.strip_prefix(&prefix) else {
        return Ok(RustWalk::Broken("the source is outside the target tree".into()));
    };
    let mut components: Vec<&str> = relative.split('/').collect();
    let last = components.pop().unwrap_or_default();
    let final_module = if last == "mod.rs" {
        match components.pop() {
            Some(parent) => parent,
            None => {
                return Ok(RustWalk::Broken(
                    "a mod.rs cannot sit directly in the target root".into(),
                ));
            }
        }
    } else {
        match last.strip_suffix(".rs") {
            Some(stem) => stem,
            None => return Ok(RustWalk::Broken("the source is not a Rust module file".into())),
        }
    };
    let mut owner_file = target.file.clone();
    let mut owner_inline: Vec<String> = Vec::new();
    let mut scope_dir = parent_dir(&owner_file).to_string();
    let mut module_path: Vec<String> = Vec::new();
    let mut chain_files = vec![target.file.clone()];
    for module in &components {
        match rust_declared_step(
            &owner_file,
            &owner_inline,
            &scope_dir,
            module,
            Some(source),
            root,
            scanned,
            probe,
            caches,
            generation,
        )? {
            RustStep::Inline => {
                owner_inline.push((*module).to_string());
                scope_dir = join_relative(&scope_dir, module);
            }
            RustStep::Routed(file) => {
                owner_file = file;
                owner_inline.clear();
                scope_dir = module_children_dir(&owner_file);
                module_path.push((*module).to_string());
                chain_files.push(owner_file.clone());
            }
            RustStep::SymbolTail | RustStep::Missing { .. } => {
                return Ok(RustWalk::Broken(
                    "the declared module chain does not reach the source".into(),
                ));
            }
            RustStep::Blocked(reason) => return Ok(RustWalk::Broken(reason)),
        }
    }
    // The final component must route to exactly the source file; the sibling
    // `name.rs` / `name/mod.rs` form must not exist alongside it.
    match rust_declared_step(
        &owner_file,
        &owner_inline,
        &scope_dir,
        final_module,
        None,
        root,
        scanned,
        probe,
        caches,
        generation,
    )? {
        RustStep::Routed(file) if file == source => {
            module_path.push(final_module.to_string());
            chain_files.push(source.to_string());
        }
        RustStep::Blocked(reason) => return Ok(RustWalk::Broken(reason)),
        _ => {
            return Ok(RustWalk::Broken(
                "the declared module chain does not reach the source".into(),
            ));
        }
    }
    Ok(RustWalk::Proven(ProvenChain {
        crate_dir: target.children_dir.clone(),
        root_file: target.file.clone(),
        children_dir: module_children_dir(source),
        module_path,
        chain_files,
    }))
}

/// Module declarations of one ancestor file, read through the captured
/// generation once per request. A vanished or unreadable-in-the-allowed-sense
/// file yields empty declarations, which simply breaks the chain.
fn rust_file_dependencies(
    file: &str,
    root: &Path,
    scanned: Option<&BTreeMap<String, SourceDependencies>>,
    caches: &mut ResolutionCaches,
    generation: &ApplicationGeneration,
) -> Result<SourceDependencies> {
    if let Some(hit) = caches.rust_files.get(file) {
        return Ok(hit.clone());
    }
    if let Some(deps) = scanned.and_then(|map| map.get(file)) {
        let deps = deps.clone();
        caches.rust_files.insert(file.to_string(), deps.clone());
        return Ok(deps);
    }
    let (absolute, _) = match file_explorer::resolve_scoped_path(root, file, false) {
        Ok(pair) => pair,
        Err(error) => return Err(error.context(format!("module file {file} unavailable"))),
    };
    let bytes = match generation.read_file_bounded(&absolute, MAX_SOURCE_BYTES as u64) {
        Ok(bytes) => bytes,
        Err(error) => {
            if allowed_source_omission(&error) {
                let empty = extract_dependencies(file, "");
                caches.rust_files.insert(file.to_string(), empty.clone());
                return Ok(empty);
            }
            return Err(error.context(format!("module file {file} unavailable")));
        }
    };
    let text = std::str::from_utf8(&bytes)
        .with_context(|| format!("module file {file} is not UTF-8"))?;
    let dependencies = extract_dependencies(file, text);
    caches.rust_files.insert(file.to_string(), dependencies.clone());
    Ok(dependencies)
}

/// Outcome of one declaration-driven module step across a path segment.
enum RustStep {
    /// The segment is an inline module: the scope descends lexically and
    /// the owner file is unchanged.
    Inline,
    /// The segment is a file-backed module whose conventional file is
    /// uniquely `file`.
    Routed(String),
    /// No declaration carries the segment: a symbol tail, never a guessed
    /// file.
    SymbolTail,
    /// The module is declared but no conventional file exists; `symlink`
    /// records a blocked-link probe.
    Missing { symlink: bool },
    /// A controlling attribute or candidate ambiguity leaves the step
    /// undecidable.
    Blocked(String),
}

/// One declaration-driven module step across `segment` from the scope whose
/// child modules live in `scope_dir`, owned by `owner_file` at inline
/// lineage `owner_inline`. An inline declaration descends lexically; a
/// file-backed one routes to the unique conventional `stem.rs` /
/// `stem/mod.rs` candidate. When `source_bound` is set (chain proof), a
/// routed file must further be an ancestor directory of that source; an
/// undeclared segment is a symbol tail, never a same-named file.
fn rust_declared_step(
    owner_file: &str,
    owner_inline: &[String],
    scope_dir: &str,
    segment: &str,
    source_bound: Option<&str>,
    root: &Path,
    scanned: Option<&BTreeMap<String, SourceDependencies>>,
    probe: &impl Fn(&str) -> Result<CandidateProbe>,
    caches: &mut ResolutionCaches,
    generation: &ApplicationGeneration,
) -> Result<RustStep> {
    let owner = rust_file_dependencies(owner_file, root, scanned, caches, generation)?;
    if let Some(declaration) = owner.module_declarations.iter().find(|declaration| {
        declaration.inline && declaration.name == segment && declaration.lineage == owner_inline
    }) {
        if declaration.path_attribute.is_some() || declaration.conservative {
            return Ok(RustStep::Blocked(
                "the module step crosses a conservative or #[path] module attribute".into(),
            ));
        }
        return Ok(RustStep::Inline);
    }
    let stem = module_file_stem(segment);
    let mut declared = false;
    let mut blocked: Option<String> = None;
    let mut routed: Option<String> = None;
    let mut saw_symlink = false;
    for declaration in owner.module_declarations.iter().filter(|declaration| {
        !declaration.inline && declaration.name == segment && declaration.lineage == owner_inline
    }) {
        declared = true;
        if declaration.path_attribute.is_some() || declaration.conservative {
            if blocked.is_none() {
                blocked = Some(
                    "the module step crosses a conservative or #[path] module attribute".into(),
                );
            }
            continue;
        }
        for candidate in [
            join_relative(scope_dir, &format!("{stem}.rs")),
            join_relative(scope_dir, &format!("{stem}/mod.rs")),
        ] {
            let on_source_path = match source_bound {
                Some(bound) => {
                    bound.starts_with(&join_relative(&module_children_dir(&candidate), ""))
                }
                None => true,
            };
            match probe(&candidate)? {
                CandidateProbe::Exists if on_source_path => {
                    if routed.is_some() {
                        return Ok(RustStep::Blocked(
                            "module file candidates are ambiguous".into(),
                        ));
                    }
                    routed = Some(candidate);
                }
                CandidateProbe::Exists | CandidateProbe::Missing => {}
                CandidateProbe::SymlinkBlocked => saw_symlink = true,
            }
        }
    }
    Ok(match routed {
        Some(file) => RustStep::Routed(file),
        None => match blocked {
            Some(reason) => RustStep::Blocked(reason),
            // A declared module without a candidate file is Missing; a
            // segment no declaration carries is a symbol tail.
            None if declared => RustStep::Missing { symlink: saw_symlink },
            None => RustStep::SymbolTail,
        },
    })
}

/// The first reason one module declaration cannot take the filesystem
/// convention, or `None` when it is declared and unambiguous.
fn rust_declaration_blocker(
    declarations: &[ModuleDeclaration],
    name: &str,
    lineage: &[String],
    inline: bool,
) -> Option<String> {
    let declared = declarations.iter().find(|declaration| {
        declaration.name == name && declaration.lineage == lineage && declaration.inline == inline
    });
    let declaration = declared?;
    if declaration.path_attribute.is_some() {
        return Some("the module chain declares #[path]; the file convention does not apply".into());
    }
    if declaration.conservative {
        return Some("the module chain carries a conservative attribute".into());
    }
    None
}

/// Every inline ancestor of an occurrence must be declared and free of
/// controlling attributes; otherwise the directory convention is unusable.
fn rust_inline_blocker(declarations: &[ModuleDeclaration], lineage: &[String]) -> Option<String> {
    for (index, name) in lineage.iter().enumerate() {
        if let Some(reason) = rust_declaration_blocker(declarations, name, &lineage[..index], true) {
            return Some(reason);
        }
    }
    None
}

/// Resolves one file-backed `mod name;` edge through the proven Cargo module
/// chain: the declaring item must be free of controlling attributes and the
/// conventional `name.rs` / `name/mod.rs` pair must be unique.
fn resolve_rust_mod(
    source: &str,
    source_deps: &SourceDependencies,
    root: &Path,
    edge: &DependencyEdge,
    scanned: Option<&BTreeMap<String, SourceDependencies>>,
    probe: &impl Fn(&str) -> Result<CandidateProbe>,
    caches: &mut ResolutionCaches,
    generation: &ApplicationGeneration,
) -> Result<MapTarget> {
    caches.rust_files.entry(source.to_string()).or_insert_with(|| source_deps.clone());
    let chain = rust_chain(source, root, scanned, probe, caches, generation)?;
    let proven = match chain {
        RustChain::Proven(proven) => proven,
        RustChain::Unproven(reason) => return Ok(unresolved(&reason)),
    };
    if let Some(reason) = rust_declaration_blocker(
        &source_deps.module_declarations,
        &edge.specifier,
        &edge.inline_module,
        false,
    ) {
        return Ok(unresolved(&reason));
    }
    if let Some(reason) = rust_inline_blocker(&source_deps.module_declarations, &edge.inline_module) {
        return Ok(unresolved(&reason));
    }
    let mut base = proven.children_dir.clone();
    for segment in &edge.inline_module {
        base = join_relative(&base, segment);
    }
    let stem = module_file_stem(&edge.specifier);
    let candidates = BTreeSet::from([
        join_relative(&base, &format!("{stem}.rs")),
        join_relative(&base, &format!("{stem}/mod.rs")),
    ]);
    match classify_candidates(candidates, probe)? {
        CandidateOutcome::File(path) => Ok(MapTarget::File { path }),
        CandidateOutcome::Missing { symlink: true } => Ok(unresolved("symlink target is not followed")),
        CandidateOutcome::Missing { symlink: false } => Ok(unresolved("no module file candidate")),
        CandidateOutcome::Multiple => Ok(unresolved("module file candidates are ambiguous")),
    }
}

/// Resolves one local `crate`/`self`/`super` use leaf by walking only declared
/// modules. The deepest proven module prefix's file is the target; a trailing
/// symbol is never validated and never mapped to a file. Non-local leaves are
/// external crates.
fn resolve_rust_use(
    source: &str,
    source_deps: &SourceDependencies,
    root: &Path,
    edge: &DependencyEdge,
    scanned: Option<&BTreeMap<String, SourceDependencies>>,
    probe: &impl Fn(&str) -> Result<CandidateProbe>,
    caches: &mut ResolutionCaches,
    generation: &ApplicationGeneration,
) -> Result<MapTarget> {
    let path = edge.specifier.as_str();
    let mut tokens = path.split("::").peekable();
    let first = tokens.next().unwrap_or("");
    if !matches!(first, "crate" | "self" | "super") {
        return Ok(MapTarget::External { specifier: path.to_string() });
    }
    caches.rust_files.entry(source.to_string()).or_insert_with(|| source_deps.clone());
    let chain = rust_chain(source, root, scanned, probe, caches, generation)?;
    let proven = match chain {
        RustChain::Proven(proven) => proven,
        RustChain::Unproven(reason) => return Ok(unresolved(&reason)),
    };
    // The occurrence's inline ancestors must be declared and free of
    // controlling attributes, exactly like the `mod` path.
    if let Some(reason) = rust_inline_blocker(&source_deps.module_declarations, &edge.inline_module) {
        return Ok(unresolved(&reason));
    }
    let mut scope_dir;
    let mut owner_file;
    let mut owner_is_root;
    let mut owner_inline: Vec<String>;
    let mut scope_path: Vec<String>;
    match first {
        "crate" => {
            scope_dir = proven.crate_dir.clone();
            owner_file = proven.root_file.clone();
            owner_is_root = true;
            owner_inline = Vec::new();
            scope_path = Vec::new();
        }
        "self" => {
            scope_dir = lineage_dir(&proven.children_dir, &edge.inline_module);
            owner_file = source.to_string();
            // Only a file-backed pop resets root-ness; the scope of the
            // crate root file itself keeps popping against the crate dir.
            owner_is_root = source == proven.root_file;
            owner_inline = edge.inline_module.clone();
            scope_path = proven.module_path.clone();
        }
        _ => {
            scope_dir = lineage_dir(&proven.children_dir, &edge.inline_module);
            owner_file = source.to_string();
            owner_is_root = source == proven.root_file;
            owner_inline = edge.inline_module.clone();
            scope_path = proven.module_path.clone();
            if !rust_pop_scope(
                &mut scope_dir,
                &mut owner_file,
                &mut owner_is_root,
                &mut owner_inline,
                &mut scope_path,
                &proven.chain_files,
                &proven.crate_dir,
            ) {
                return Ok(unresolved("super extends above the crate root"));
            }
        }
    }
    let mut scope_chain = proven.chain_files.clone();
    while let Some(token) = tokens.next() {
        match token {
            "super" => {
                if !rust_pop_scope(
                    &mut scope_dir,
                    &mut owner_file,
                    &mut owner_is_root,
                    &mut owner_inline,
                    &mut scope_path,
                    &scope_chain,
                    &proven.crate_dir,
                ) {
                    return Ok(unresolved("super extends above the crate root"));
                }
            }
            "self" | "crate" => return Ok(unresolved("a self or crate keyword appears mid-path")),
            "*" if tokens.peek().is_none() => break,
            "*" => return Ok(unresolved("a wildcard appears mid-path")),
            segment => {
                match rust_declared_step(
                    &owner_file,
                    &owner_inline,
                    &scope_dir,
                    segment,
                    None,
                    root,
                    scanned,
                    probe,
                    caches,
                    generation,
                )? {
                    RustStep::Inline => {
                        owner_inline.push(segment.to_string());
                        scope_dir = join_relative(&scope_dir, segment);
                    }
                    RustStep::Routed(file) => {
                        scope_path.push(segment.to_string());
                        scope_chain.push(file.clone());
                        owner_file = file;
                        owner_is_root = false;
                        owner_inline.clear();
                        scope_dir = module_children_dir(&owner_file);
                    }
                    RustStep::SymbolTail => break, // the deepest proven module prefix is the scope itself
                    RustStep::Blocked(reason) => return Ok(unresolved(&reason)),
                    RustStep::Missing { symlink: true } => {
                        return Ok(unresolved("symlink target is not followed"));
                    }
                    RustStep::Missing { symlink: false } => {
                        return Ok(unresolved(&format!(
                            "the declared module {segment} has no module file"
                        )));
                    }
                }
            }
        }
    }
    Ok(MapTarget::File { path: owner_file })
}

/// Pops one module level from a use scope: an inline segment first, then a
/// file-backed one through the proven chain. `false` means the pop left the
/// crate.
fn rust_pop_scope(
    scope_dir: &mut String,
    owner_file: &mut String,
    owner_is_root: &mut bool,
    owner_inline: &mut Vec<String>,
    scope_path: &mut Vec<String>,
    chain_files: &[String],
    crate_dir: &str,
) -> bool {
    if owner_inline.pop().is_some() {
        let base =
            if *owner_is_root { crate_dir.to_string() } else { module_children_dir(owner_file) };
        *scope_dir = lineage_dir(&base, owner_inline);
        return true;
    }
    if scope_path.pop().is_none() {
        return false;
    }
    let owner = chain_files
        .get(scope_path.len())
        .cloned()
        .expect("the chain carries a file for every walked module");
    *owner_file = owner;
    *owner_is_root = scope_path.is_empty();
    *scope_dir =
        if *owner_is_root { crate_dir.to_string() } else { module_children_dir(owner_file) };
    true
}

fn resolve_edge(
    source: &str,
    source_deps: &SourceDependencies,
    root: &Path,
    edge: &DependencyEdge,
    scanned: Option<&BTreeMap<String, SourceDependencies>>,
    foundry: Option<&FoundryDefault>,
    probe: &impl Fn(&str) -> Result<CandidateProbe>,
    caches: &mut ResolutionCaches,
    generation: &ApplicationGeneration,
) -> Result<MapTarget> {
    if edge.kind == EdgeKind::Import && source_deps.language == MapLanguage::Go {
        return resolve_go_import(&edge.specifier, root, scanned, caches, generation);
    }
    if edge.kind == EdgeKind::Mod {
        return resolve_rust_mod(source, source_deps, root, edge, scanned, probe, caches, generation);
    }
    if edge.kind == EdgeKind::Use {
        return resolve_rust_use(source, source_deps, root, edge, scanned, probe, caches, generation);
    }
    let specifier = &edge.specifier;
    let relative = specifier.starts_with("./") || specifier.starts_with("../");
    if !relative && edge.kind != EdgeKind::SolImport {
        return Ok(if specifier.starts_with('/') {
            unresolved("absolute imports are outside relative resolution")
        } else if matches!(
            source_deps.language,
            MapLanguage::TypeScript | MapLanguage::Tsx | MapLanguage::JavaScript
        ) {
            resolve_ts_bare(specifier, parent_dir(source), root, probe, caches, generation)?
        } else {
            MapTarget::External { specifier: specifier.clone() }
        });
    }
    if specifier.starts_with('/') || specifier.contains('\\') {
        return Ok(unresolved("absolute or platform-specific import is not supported"));
    }
    if !relative {
        if let Some(foundry) = foundry {
            return resolve_remapped(root, specifier, foundry, probe);
        }
    }
    // Without a Foundry profile, Solidity non-relative imports are exact
    // source-unit paths from the workspace root. Package resolution is
    // never guessed.
    let joined = if relative {
        Path::new(source).parent().unwrap_or(Path::new("")).join(specifier)
    } else {
        Path::new(specifier).to_path_buf()
    };
    let Ok((_, base)) = file_explorer::resolve_scoped_path(Path::new("/"), &joined.to_string_lossy(), false) else {
        return Ok(unresolved("relative import escapes workspace"));
    };
    Ok(match classify_candidates(relative_candidates(&base, edge.kind == EdgeKind::SolImport), probe)? {
        CandidateOutcome::File(path) => match scanned {
            Some(sources) if !sources.contains_key(&path) => {
                unresolved("target was not readable within the bounded source scan")
            }
            _ => MapTarget::File { path },
        },
        CandidateOutcome::Missing { symlink: true } => unresolved("symlink target is not followed"),
        CandidateOutcome::Missing { symlink: false } => unresolved("no relative file candidate"),
        CandidateOutcome::Multiple => unresolved("multiple relative file candidates"),
    })
}

/// Resolve a non-relative Solidity import through the longest matching
/// Foundry remapping. The match must end on a `/` boundary; the spliced
/// target is verified against the real workspace root — an absolute or
/// escaping splice is rejected without any existence probe — and a
/// missing target is reported rather than falling back to another path.
fn resolve_remapped(
    root: &Path,
    specifier: &str,
    foundry: &FoundryDefault,
    probe: &impl Fn(&str) -> Result<CandidateProbe>,
) -> Result<MapTarget> {
    let mut best: Option<(&str, &str, &str)> = None;
    for (key, value) in &foundry.remappings {
        if let Some(remainder) = specifier.strip_prefix(key.as_str()) {
            let on_boundary = key.ends_with('/') || remainder.is_empty() || remainder.starts_with('/');
            if on_boundary && best.is_none_or(|(longest, _, _)| key.len() > longest.len()) {
                best = Some((key, value, remainder));
            }
        }
    }
    let Some((_, value, remainder)) = best else {
        return Ok(unresolved("no foundry remapping"));
    };
    let spliced = format!("{value}{remainder}");
    let Ok((_, base)) = file_explorer::resolve_scoped_path(root, &spliced, false) else {
        return Ok(unresolved("remapped target escapes workspace"));
    };
    Ok(match probe(&base)? {
        CandidateProbe::Exists => MapTarget::File { path: base },
        CandidateProbe::Missing => unresolved("remapped target not found"),
        CandidateProbe::SymlinkBlocked => unresolved("symlink target is not followed"),
    })
}

fn probe_candidate(generation: &ApplicationGeneration, root: &Path, candidate: &str) -> Result<CandidateProbe> {
    let (absolute, _) = file_explorer::resolve_scoped_path(root, candidate, false)?;
    match generation.read_file_bounded(&absolute, 1) {
        Ok(_) => Ok(CandidateProbe::Exists),
        Err(error) => match error.downcast_ref::<FsError>() {
            Some(FsError::ReadLimitExceeded { .. }) => Ok(CandidateProbe::Exists),
            Some(FsError::UnsupportedEntryType) => Ok(CandidateProbe::Missing),
            Some(fs_error) if is_symlink_eloop(fs_error) => Ok(CandidateProbe::SymlinkBlocked),
            Some(FsError::System { kind, .. }) if matches!(kind, std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory) => Ok(CandidateProbe::Missing),
            _ => Err(error.context(format!("dependency candidate unavailable: {candidate}"))),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_agent::ThinkingLevel;
    use pi_coding::{ResourceDiscovery, SessionOptions, ToolSelection, WorkspaceRoots};

    async fn application(cwd: &Path) -> (Application, tempfile::TempDir) {
        pi_ai::providers::register_default_faux();
        let model = pi_ai::get_model("faux", "faux-1").expect("faux model");
        let workspace = WorkspaceRoots::new(cwd, std::iter::empty::<&Path>()).expect("roots");
        let session = pi_coding::Session::new_with_additional_tools_filtered_discovery_workspace_and_uri(
            SessionOptions {
                model: Some(model), cwd: cwd.to_path_buf(), system_prompt: String::new(),
                thinking_level: ThinkingLevel::Off, api_key: "faux".into(),
                compaction: None, stream_options: Default::default(), tools: Some(Vec::new()),
                before_tool_call: None, after_tool_call: None, stream_fn: None,
                auth_resolver: None, auth_manager: None,
            }, Vec::new(), ToolSelection::default(), ResourceDiscovery::Disabled, workspace, None,
        ).expect("session");
        let agent = tempfile::tempdir().expect("agent dir");
        std::fs::write(agent.path().join("settings.json"), r#"{"sandbox":{"mode":"danger-full-access"}}"#).expect("settings");
        let session_dir = cwd.join(".pi-test-sessions");
        std::fs::create_dir_all(&session_dir).expect("sessions");
        session.set_session_dir(session_dir);
        let mut options = pi_coding::ResourceManagerOptions::new(cwd);
        options.agent_dir = agent.path().to_path_buf();
        options.project_trust_override = Some(true);
        options.disable_extensions = true;
        options.disable_skills = true;
        options.disable_prompt_templates = true;
        options.disable_themes = true;
        options.disable_context_files = true;
        session.attach_resources(pi_coding::ResourceManager::new(options).expect("resources")).await.expect("attach");
        let app = Application::from_runtime_candidate(pi_coding::ApplicationRuntimeCandidate::new(session)).await.expect("app");
        (app, agent)
    }

    #[tokio::test]
    async fn fixture_edges_reverse_docs_and_omissions() {
        let dir = tempfile::tempdir().expect("workspace");
        std::fs::write(dir.path().join("a.ts"), "// import './fake';\nimport './b';\nimport 'package';\nimport './missing';\nimport('./dynamic');\n/** Actual source doc */\nexport function run() {}\n").expect("a");
        std::fs::write(dir.path().join("b.ts"), "export const b = 1;\n").expect("b");
        std::fs::write(dir.path().join("c.ts"), "import './a';\n").expect("c");
        std::fs::write(dir.path().join("notes.txt"), "unsupported").expect("notes");
        let (app, _agent) = application(dir.path()).await;
        let graph = dependency_map(&app, "a.ts").expect("graph");
        assert_eq!(graph.outgoing.len(), 3);
        assert_eq!(graph.outgoing[0].line, 2);
        assert_eq!(graph.outgoing[0].target, MapTarget::File { path: "b.ts".into() });
        assert!(matches!(graph.outgoing[1].target, MapTarget::External { .. }));
        assert!(matches!(graph.outgoing[2].target, MapTarget::Unresolved { .. }));
        assert_eq!(graph.incoming.len(), 1);
        assert_eq!(graph.incoming[0].source, "c.ts");
        assert_eq!(graph.coverage.dynamic_omitted, 1);
        assert!(graph.coverage.unsupported_files >= 1);
        let selected = graph.nodes.iter().find(|node| node.path == "a.ts").expect("selected");
        assert_eq!(selected.purpose[0].text, "Actual source doc");
        assert_eq!(selected.purpose[0].line, 7);
        assert!(graph.coverage.incomplete && graph.coverage.visible_entries_only);
    }

    #[tokio::test]
    async fn ambiguity_oversize_and_scope_fail_closed() {
        let dir = tempfile::tempdir().expect("workspace");
        std::fs::write(dir.path().join("a.ts"), "import './b';").expect("a");
        std::fs::write(dir.path().join("b.ts"), "export {};").expect("ts");
        std::fs::write(dir.path().join("b.js"), "export {};").expect("js");
        std::fs::write(dir.path().join("huge.ts"), vec![b' '; MAX_SOURCE_BYTES + 1]).expect("huge");
        let (app, _agent) = application(dir.path()).await;
        let graph = dependency_map(&app, "a.ts").expect("graph");
        assert!(matches!(graph.outgoing[0].target, MapTarget::Unresolved { .. }));
        assert_eq!(graph.coverage.unreadable_files, 1);
        assert!(dependency_map(&app, "huge.ts").is_err());
        assert!(dependency_map(&app, "../outside.ts").is_err());
        let outside = tempfile::NamedTempFile::new().expect("outside");
        assert!(dependency_map(&app, outside.path().to_str().expect("path")).is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_files_and_directories_never_supply_edges() {
        let dir = tempfile::tempdir().expect("workspace");
        let outside = tempfile::tempdir().expect("outside");
        std::fs::write(outside.path().join("secret.ts"), "export const secret = 1;").expect("secret");
        std::fs::create_dir(dir.path().join("linked")).expect("internal dir");
        std::fs::write(dir.path().join("linked/local.ts"), "export const local = 1;").expect("local");
        std::fs::write(dir.path().join("plain.ts"), "export const plain = 1;").expect("plain");
        std::fs::write(dir.path().join("a.ts"), "import './link'; import './dirlink/local'; import './plain'; import './ext';").expect("a");
        std::os::unix::fs::symlink("plain.ts", dir.path().join("link.ts")).expect("internal file link");
        std::os::unix::fs::symlink("linked", dir.path().join("dirlink")).expect("internal directory link");
        std::os::unix::fs::symlink(outside.path().join("secret.ts"), dir.path().join("ext.ts")).expect("external file link");
        let (app, _agent) = application(dir.path()).await;
        assert!(dependency_map(&app, "link.ts").is_err(), "selecting the link itself stays an authority error");
        assert!(dependency_map(&app, "dirlink/local.ts").is_err(), "selecting through the link stays an authority error");
        assert!(dependency_map(&app, "ext.ts").is_err(), "selecting an external link stays an authority error");
        let graph = dependency_map(&app, "a.ts").expect("symlink edges stay unresolved");
        assert_eq!(graph.outgoing.len(), 4);
        assert_eq!(graph.outgoing[0].target, MapTarget::Unresolved { reason: "symlink target is not followed".into() });
        assert_eq!(graph.outgoing[1].target, MapTarget::Unresolved { reason: "symlink target is not followed".into() });
        assert_eq!(graph.outgoing[2].target, MapTarget::File { path: "plain.ts".into() });
        assert_eq!(graph.outgoing[3].target, MapTarget::Unresolved { reason: "symlink target is not followed".into() });
        let hop = one_hop_dependencies(
            &app.capture_generation(),
            dir.path(),
            read_source(&app.capture_generation(), dir.path(), "a.ts").expect("one read"),
        ).expect("one hop classifies the same way");
        assert_eq!(hop.outgoing[0].target, graph.outgoing[0].target);
        assert_eq!(hop.outgoing[2].target, MapTarget::File { path: "plain.ts".into() });
        assert_eq!(hop.outgoing[3].target, graph.outgoing[3].target);
    }


    #[tokio::test]
    async fn file_scan_bound_is_explicit() {
        let dir = tempfile::tempdir().expect("workspace");
        for index in 0..=MAX_SCAN_FILES {
            std::fs::write(dir.path().join(format!("f{index:05}.ts")), "export {};").expect("fixture");
        }
        let (app, _agent) = application(dir.path()).await;
        let graph = dependency_map(&app, "f00000.ts").expect("graph");
        assert!(graph.coverage.truncated);
        assert!(graph.coverage.scanned_files <= MAX_SCAN_FILES);
    }
    #[tokio::test]
    async fn rust_context_is_explicitly_unresolved_not_filename_guessed() {
        let dir = tempfile::tempdir().expect("workspace");
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        ).expect("manifest");
        std::fs::create_dir_all(dir.path().join("src")).expect("src");
        std::fs::write(dir.path().join("src/lib.rs"), "mod nested { mod child; use super::Thing; }\n").expect("lib");
        std::fs::write(dir.path().join("src/child.rs"), "pub struct Thing;\n").expect("child");
        let (app, _agent) = application(dir.path()).await;
        let generation = app.capture_generation();
        let selected = read_source(&generation, dir.path(), "src/lib.rs").expect("selected");
        let hop = one_hop_dependencies(&generation, dir.path(), selected).expect("hop");
        assert_eq!(hop.outgoing.len(), 2);
        assert!(matches!(
            &hop.outgoing[0].target,
            MapTarget::Unresolved { .. }
        ), "the inline-scoped mod never binds the same-named src/child.rs bait");
        assert_eq!(
            hop.outgoing[1].target,
            MapTarget::File { path: "src/lib.rs".into() },
            "the symbol tail lands on the deepest proven scope file, never the bait"
        );
        assert!(hop.outgoing.iter().all(|edge| {
            !matches!(&edge.target, MapTarget::File { path } if path == "src/child.rs")
        }));
    }
    #[tokio::test]
    async fn solidity_exact_import_and_incomplete_source_are_reported() {
        let dir = tempfile::tempdir().expect("workspace");
        std::fs::write(dir.path().join("A.sol"), "// import './Fake.sol';\nimport './B.sol';\ncontract A {}\n").expect("a");
        std::fs::write(dir.path().join("B.sol"), "contract B {}\n").expect("b");
        std::fs::write(dir.path().join("broken.ts"), "export function broken( {").expect("broken");
        std::fs::write(dir.path().join(".gitignore"), "ignored.ts\n").expect("ignore");
        std::fs::write(dir.path().join("ignored.ts"), "import './A.sol';").expect("ignored");
        let (app, _agent) = application(dir.path()).await;
        let graph = dependency_map(&app, "A.sol").expect("graph");
        assert_eq!(graph.outgoing.len(), 1);
        assert_eq!(graph.outgoing[0].target, MapTarget::File { path: "B.sol".into() });
        assert_eq!(graph.outgoing[0].line, 2);
        assert_eq!(graph.coverage.incomplete_files, 1);
        assert!(graph.incoming.is_empty());
    }
    #[tokio::test]
    async fn explicit_extension_never_matches_appended_extension() {
        let dir = tempfile::tempdir().expect("workspace");
        std::fs::write(dir.path().join("a.ts"), "import './b.js';").expect("a");
        std::fs::write(dir.path().join("b.js.ts"), "export {};").expect("misleading candidate");
        let (app, _agent) = application(dir.path()).await;
        let graph = dependency_map(&app, "a.ts").expect("graph");
        assert!(matches!(graph.outgoing[0].target, MapTarget::Unresolved { .. }));
        std::fs::write(dir.path().join("b.js"), "export {};").expect("exact candidate");
        let graph = dependency_map(&app, "a.ts").expect("graph");
        assert_eq!(graph.outgoing[0].target, MapTarget::File { path: "b.js".into() });
    }

    #[tokio::test]
    async fn unexpected_probe_errors_propagate_and_authority_errors_are_not_omissions() {
        let dir = tempfile::tempdir().expect("workspace");
        std::fs::write(dir.path().join("a.ts"), "import './b';").expect("a");
        let (app, _agent) = application(dir.path()).await;
        let sources = BTreeMap::from([("a.ts".into(), extract_dependencies("a.ts", "import './b';"))]);
        let probe = |_: &str| -> Result<CandidateProbe> { Err(FsError::Denied.into()) };
        let mut caches = ResolutionCaches::default();
        let generation = app.capture_generation();
        let error = assemble(
            "a.ts",
            dir.path(),
            &sources,
            None,
            MapCoverage::default(),
            &probe,
            &mut caches,
            &generation,
        )
        .expect_err("denied probe");
        assert!(matches!(error.downcast_ref::<FsError>(), Some(FsError::Denied)));
        assert!(!allowed_source_omission(&error));
        assert!(!allowed_source_omission(&anyhow!("unexpected generation failure")));
        assert!(allowed_source_omission(&FsError::UnsupportedEntryType.into()));
    }
    #[tokio::test]
    async fn retained_generation_remains_the_authority_after_reload() {
        let dir = tempfile::tempdir().expect("workspace");
        std::fs::write(dir.path().join("a.ts"), "import './b';").expect("a");
        std::fs::write(dir.path().join("b.ts"), "export {};").expect("b");
        let (app, _agent) = application(dir.path()).await;
        let retained = app.capture_generation();
        app.reload().await.expect("reload");
        assert!(!std::sync::Arc::ptr_eq(&retained, &app.capture_generation()));
        let selected = read_source(&retained, dir.path(), "a.ts").expect("retained source");
        assert_eq!(selected.source.edges.len(), 1);
        assert!(matches!(probe_candidate(&retained, dir.path(), "b.ts").expect("retained probe"), CandidateProbe::Exists));
        let listing = file_explorer::list_with_generation(&retained, dir.path(), "").expect("retained listing");
        assert!(listing.entries.iter().any(|entry| entry.path == "a.ts"));
    }

    /// One-read regression for the Ask path: `read_source` reads the
    /// selected file exactly once, and both consumers — the file body page
    /// and the classified outgoing edges — are built from that same read.
    /// Disk changes after the read cannot split the two.
    #[tokio::test]
    async fn selected_body_and_edges_both_stay_at_the_single_read() {
        let dir = tempfile::tempdir().expect("workspace");
        // Body lines must stay valid TypeScript: the AST extractor fails
        // closed (empty edges, ParseStatus::Incomplete) on any parse error.
        std::fs::write(dir.path().join("a.ts"), "import './b';\n// OLD BODY\n").expect("a");
        std::fs::write(dir.path().join("b.ts"), "export const b = 1;\n").expect("b");
        let (app, _agent) = application(dir.path()).await;
        let generation = app.capture_generation();
        let selected = read_source(&generation, dir.path(), "a.ts").expect("selected");

        std::fs::write(dir.path().join("a.ts"), "import './c';\n// NEW BODY\n").expect("a2");
        std::fs::write(dir.path().join("c.ts"), "export const c = 1;\n").expect("c");

        let page = file_explorer::text_page(
            &selected.bytes[..],
            &selected.relative,
            0,
            None,
            file_explorer::MAX_PAGE_CONTENT_BYTES,
        ).expect("body page");
        assert_eq!(page.lines, vec!["import './b';", "// OLD BODY"]);
        assert_eq!(page.total_lines, 2);

        let hop = one_hop_dependencies(&generation, dir.path(), selected).expect("hop");
        assert_eq!(hop.outgoing.len(), 1);
        assert_eq!(hop.outgoing[0].specifier, "./b");
        assert_eq!(hop.outgoing[0].target, MapTarget::File { path: "b.ts".into() });
    }

    #[tokio::test]
    async fn directory_graph_stays_inside_the_opened_directory() {
        let dir = tempfile::tempdir().expect("workspace");
        std::fs::create_dir(dir.path().join("src")).expect("src");
        std::fs::write(dir.path().join("src/a.ts"), "import './b';\nimport 'left-pad';\n").expect("a");
        std::fs::write(dir.path().join("src/b.ts"), "import './a';\nexport const b = 1;\n").expect("b");
        std::fs::write(dir.path().join("other.ts"), "import './src/a';\n").expect("other");
        let (app, _agent) = application(dir.path()).await;
        let graph = dependency_map(&app, "src").expect("directory graph");
        assert_eq!(graph.path, "src");
        assert!(graph.incoming.is_empty());
        assert!(graph.nodes.iter().any(|node| node.path == "src/a.ts"));
        assert!(graph.nodes.iter().any(|node| node.path == "src/b.ts"));
        assert!(graph.nodes.iter().all(|node| node.path.starts_with("src/")));
        assert!(graph.outgoing.iter().any(|edge| {
            edge.source == "src/a.ts" && edge.target == MapTarget::File { path: "src/b.ts".into() }
        }));
        assert!(graph.outgoing.iter().all(|edge| edge.source.starts_with("src/")),
            "only scope files produce edges");
        assert!(graph.outgoing.iter().any(|edge| edge.source == "src/a.ts"
            && matches!(&edge.target, MapTarget::External { specifier } if specifier == "left-pad")),
            "external edges are kept, not omitted");
        assert!(!graph.outgoing.iter().any(|edge| edge.source == "other.ts"));
    }

    /// Isolated Foundry fixture (tempfile only, per the frozen interface):
    /// nested src Solidity, remappings that cross into lib/, and one TS
    /// bare specifier. No psy-contracts checkout is ever read.
    fn write_foundry_fixture(dir: &Path) {
        std::fs::create_dir_all(dir.join("src/nested")).expect("src tree");
        std::fs::create_dir_all(dir.join("lib/long")).expect("lib tree");
        std::fs::write(
            dir.join("foundry.toml"),
            "[profile.default]\nsrc = \"src\"\nremappings = [\"oz/=lib/short/\", \"oz/token/=lib/long/\"]\n",
        ).expect("foundry.toml");
        std::fs::write(
            dir.join("src/A.sol"),
            "// import './Fake.sol';\nimport \"./nested/B.sol\";\nimport \"oz/token/T.sol\";\nimport \"oz/Missing.sol\";\nimport \"no-such/Pkg.sol\";\ncontract A {}\n",
        ).expect("A.sol");
        std::fs::write(dir.join("src/nested/B.sol"), "contract B {}\n").expect("B.sol");
        std::fs::write(dir.join("lib/long/T.sol"), "contract T {}\n").expect("T.sol");
        std::fs::write(dir.join("src/note.ts"), "import 'left-pad';\n").expect("note.ts");
    }

    #[tokio::test]
    async fn foundry_default_scope_keeps_request_path_and_classifies_remaps() {
        let dir = tempfile::tempdir().expect("workspace");
        write_foundry_fixture(dir.path());
        let (app, _agent) = application(dir.path()).await;

        // Request "" opens the Foundry src scope and echoes the request path.
        let root_graph = dependency_map(&app, "").expect("root graph");
        assert_eq!(root_graph.path, "");
        assert!(root_graph.nodes.iter().any(|node| node.path == "src/A.sol"), "src/A.sol must be a scope node");
        assert!(root_graph.nodes.iter().any(|node| node.path == "src/nested/B.sol"), "nested Solidity must be recursed into the src scope");
        assert!(root_graph.nodes.iter().all(|node| node.path.starts_with("src/")), "nodes remain scope files only");
        assert!(root_graph.outgoing.iter().any(|edge| {
            edge.source == "src/A.sol" && edge.target == MapTarget::File { path: "src/nested/B.sol".into() }
        }), "the relative import edge must survive the scope graph");

        // Request "src" recurses the nested tree and keeps valid edges that
        // leave the scope without making those targets nodes.
        let graph = dependency_map(&app, "src").expect("src graph");
        assert_eq!(graph.path, "src");
        assert!(graph.nodes.iter().any(|node| node.path == "src/nested/B.sol"), "nested Solidity must be a node of the src scope");
        assert!(graph.nodes.iter().all(|node| node.path.starts_with("src/")), "nodes remain scope files only");
        assert!(graph.outgoing.iter().any(|edge| {
            edge.source == "src/A.sol" && edge.target == MapTarget::File { path: "src/nested/B.sol".into() }
        }));
        assert!(graph.outgoing.iter().any(|edge| {
            edge.source == "src/A.sol" && edge.target == MapTarget::File { path: "lib/long/T.sol".into() }
        }), "an in-workspace File target outside the scope is kept, not omitted");
        assert!(graph.outgoing.iter().any(|edge| edge.source == "src/A.sol"
            && matches!(edge.target, MapTarget::Unresolved { .. })), "unresolved edges are kept, not dropped");
        assert!(graph.outgoing.iter().any(|edge| edge.source == "src/note.ts"
            && matches!(edge.target, MapTarget::External { .. })), "external edges are kept, not dropped");
        assert!(graph.nodes.iter().all(|node| node.path != "lib/long/T.sol"), "an out-of-scope target must not become a node");

        // The one-hop file map classifies remapped Solidity imports.
        let file_graph = dependency_map(&app, "src/A.sol").expect("file graph");
        assert_eq!(file_graph.outgoing.len(), 4, "no edge is dropped");
        assert_eq!(file_graph.outgoing[0].specifier, "./nested/B.sol");
        assert_eq!(file_graph.outgoing[0].target, MapTarget::File { path: "src/nested/B.sol".into() });
        assert_eq!(file_graph.outgoing[1].specifier, "oz/token/T.sol");
        assert_eq!(file_graph.outgoing[1].target, MapTarget::File { path: "lib/long/T.sol".into() }, "the longer remapping prefix wins over the shorter one's spliced path");
        assert_eq!(file_graph.outgoing[2].specifier, "oz/Missing.sol");
        assert_eq!(file_graph.outgoing[2].target, MapTarget::Unresolved { reason: "remapped target not found".into() }, "a missing remapped target is unresolved, never external");
        assert_eq!(file_graph.outgoing[3].specifier, "no-such/Pkg.sol");
        assert_eq!(file_graph.outgoing[3].target, MapTarget::Unresolved { reason: "no foundry remapping".into() });
        assert!(file_graph.incoming.is_empty());

        // Opening lib stays a single listing level even with a default profile.
        let lib_graph = dependency_map(&app, "lib").expect("lib graph");
        assert_eq!(lib_graph.path, "lib");
        assert!(lib_graph.nodes.iter().all(|node| node.path != "lib/long/T.sol"), "opening lib stays one level");

        let note_graph = dependency_map(&app, "src/note.ts").expect("note graph");
        assert!(matches!(&note_graph.outgoing[0].target, MapTarget::External { specifier } if specifier == "left-pad"));
    }

    #[tokio::test]
    async fn foundry_absent_keeps_relative_files_and_no_candidate_reason() {
        let dir = tempfile::tempdir().expect("workspace");
        std::fs::write(dir.path().join("A.sol"), "import \"./B.sol\";\nimport \"oz/T.sol\";\ncontract A {}\n").expect("A");
        std::fs::write(dir.path().join("B.sol"), "contract B {}\n").expect("B");
        let (app, _agent) = application(dir.path()).await;
        let graph = dependency_map(&app, "A.sol").expect("graph");
        assert_eq!(graph.outgoing[0].target, MapTarget::File { path: "B.sol".into() });
        assert_eq!(graph.outgoing[1].target, MapTarget::Unresolved { reason: "no relative file candidate".into() }, "without foundry.toml a Solidity package import is never guessed");
    }

    #[tokio::test]
    async fn foundry_loader_failures_fail_closed_with_context() {
        let build = |toml: &str| -> tempfile::TempDir {
            let dir = tempfile::tempdir().expect("workspace");
            std::fs::write(dir.path().join("foundry.toml"), toml).expect("foundry.toml");
            std::fs::write(dir.path().join("A.sol"), "import \"./B.sol\";\ncontract A {}\n").expect("A");
            std::fs::write(dir.path().join("B.sol"), "contract B {}\n").expect("B");
            dir
        };
        for (name, toml) in [
            ("malformed toml", "[profile.default\nthis is not toml =====\n"),
            ("only ci profile", "[profile.ci]\nsrc = \"src\"\n"),
            ("duplicate remapping prefix", "[profile.default]\nremappings = [\"oz/=lib/oz/\", \"oz/=lib/other/\"]\n"),
        ] {
            let dir = build(toml);
            let (app, _agent) = application(dir.path()).await;
            let error = dependency_map(&app, "A.sol")
                .expect_err("{name} must fail closed instead of returning an empty graph");
            assert!(!error.to_string().is_empty(), "{name} must carry context");
        }
    }

    #[tokio::test]
    async fn foundry_scope_truncation_covers_the_nested_tree() {
        let dir = tempfile::tempdir().expect("workspace");
        std::fs::write(dir.path().join("foundry.toml"), "[profile.default]\nsrc = \"src\"\n").expect("foundry.toml");
        std::fs::create_dir_all(dir.path().join("src/nested")).expect("src tree");
        // 79 top-level plus 2 nested supported sources: only the recursed
        // scope exceeds the 80-file directory budget.
        for index in 0..79 {
            std::fs::write(dir.path().join(format!("src/f{index:02}.sol")), format!("contract F{index} {{}}\n")).expect("fixture");
        }
        for index in 0..2 {
            std::fs::write(dir.path().join(format!("src/nested/g{index}.sol")), format!("contract G{index} {{}}\n")).expect("fixture");
        }
        let (app, _agent) = application(dir.path()).await;
        let graph = dependency_map(&app, "src").expect("graph");
        assert!(graph.coverage.truncated, "81 supported sources across the nested scope exceed the 80 budget");
    }
    /// An absolute remapping target must be rejected against the real
    /// workspace root. Resolving the splice against `/` would wash
    /// `/outside/T.sol` into the relative path `outside/T.sol` and bind the
    /// same-named decoy inside the workspace; the fix never probes it.
    #[tokio::test]
    async fn absolute_remapped_escape_never_binds_workspace_decoy() {
        let dir = tempfile::tempdir().expect("workspace");
        std::fs::write(
            dir.path().join("foundry.toml"),
            "[profile.default]\nsrc = \"src\"\nremappings = [\"up/=/outside/\"]\n",
        ).expect("foundry.toml");
        std::fs::create_dir_all(dir.path().join("src")).expect("src");
        std::fs::write(dir.path().join("src/A.sol"), "import \"up/T.sol\";\ncontract A {}\n").expect("a");
        std::fs::create_dir_all(dir.path().join("outside")).expect("decoy tree");
        std::fs::write(dir.path().join("outside/T.sol"), "contract Decoy {}\n").expect("decoy");
        let (app, _agent) = application(dir.path()).await;
        let graph = dependency_map(&app, "src/A.sol").expect("graph");
        assert_eq!(
            graph.outgoing[0].target,
            MapTarget::Unresolved { reason: "remapped target escapes workspace".into() },
            "the absolute splice is rejected without probing the same-named decoy"
        );
        assert!(!graph.outgoing.iter().any(|edge| {
            matches!(&edge.target, MapTarget::File { path } if path == "outside/T.sol")
        }));
    }
    /// Pure resolver proof the RPC-level decoy test cannot give: the BFS of
    /// `dependency_map` reads the decoy as part of its own scan, so only a
    /// direct `resolve_remapped` call with a counting probe closure shows
    /// that an escaping splice is rejected before any existence probe —
    /// even when the probe would answer "exists".
    #[test]
    fn absolute_remapped_escape_probes_nothing() {
        let foundry = FoundryDefault {
            src: "src".into(),
            remappings: BTreeMap::from([("up/".into(), "/outside/".into())]),
        };
        let probes = std::cell::Cell::new(0usize);
        let target = resolve_remapped(
            Path::new("/workspace"),
            "up/T.sol",
            &foundry,
            &|_| {
                probes.set(probes.get() + 1);
                Ok(CandidateProbe::Exists)
            },
        ).expect("resolved");
        assert_eq!(target, MapTarget::Unresolved { reason: "remapped target escapes workspace".into() });
        assert_eq!(probes.get(), 0, "an escaping splice never reaches the existence probe");
    }

    #[tokio::test]
    async fn ts_paths_alias_resolves_exact_before_wildcard() {
        let dir = tempfile::tempdir().expect("workspace");
        std::fs::write(
            dir.path().join("tsconfig.json"),
            "{\n// line comment\n/* block comment */\n\"compilerOptions\": {\n\"baseUrl\": \".\",\n\"paths\": {\n\"exact\": [\"./src/exact.ts\"],\n\"@app/*\": [\"./src/app/*\"],\n},\n},\n}",
        ).expect("tsconfig");
        std::fs::create_dir_all(dir.path().join("src/app")).expect("app dir");
        std::fs::write(dir.path().join("src/app/x.ts"), "export const x = 1;\n").expect("x");
        std::fs::write(dir.path().join("src/exact.ts"), "export const e = 1;\n").expect("exact");
        std::fs::write(dir.path().join("a.ts"), "import '@app/x';\nimport 'exact';\nimport 'unknown';\n").expect("a");
        let (app, _agent) = application(dir.path()).await;
        let graph = dependency_map(&app, "a.ts").expect("graph");
        assert_eq!(graph.outgoing.len(), 3);
        assert_eq!(graph.outgoing[0].target, MapTarget::File { path: "src/app/x.ts".into() });
        assert_eq!(graph.outgoing[1].target, MapTarget::File { path: "src/exact.ts".into() });
        assert_eq!(graph.outgoing[2].target, MapTarget::External { specifier: "unknown".into() });
    }

    #[tokio::test]
    async fn ts_extends_child_paths_replace_parent_and_keep_origin() {
        let dir = tempfile::tempdir().expect("workspace");
        std::fs::write(
            dir.path().join("tsconfig.json"),
            "{\"compilerOptions\":{\"baseUrl\":\".\",\"paths\":{\"old/*\":[\"./legacy/*\"]}}}",
        ).expect("root config");
        std::fs::create_dir_all(dir.path().join("web/src")).expect("web");
        std::fs::write(
            dir.path().join("web/tsconfig.json"),
            "{\"extends\":\"../tsconfig.json\",\"compilerOptions\":{\"baseUrl\":\".\",\"paths\":{\"web/*\":[\"./src/*\"]}}}",
        ).expect("child config");
        std::fs::create_dir_all(dir.path().join("legacy")).expect("legacy");
        std::fs::write(dir.path().join("legacy/keep.ts"), "export {};\n").expect("legacy keep");
        std::fs::write(dir.path().join("web/src/w.ts"), "export {};\n").expect("w");
        std::fs::write(dir.path().join("web/a.ts"), "import 'web/w';\nimport 'old/keep';\n").expect("a");
        std::fs::write(dir.path().join("b.ts"), "import 'old/keep';\n").expect("b");
        let (app, _agent) = application(dir.path()).await;
        let graph = dependency_map(&app, "web/a.ts").expect("child graph");
        assert_eq!(graph.outgoing[0].target, MapTarget::File { path: "web/src/w.ts".into() });
        assert!(
            matches!(&graph.outgoing[1].target, MapTarget::External { specifier } if specifier == "old/keep"),
            "the child paths option replaces the parent's wholesale"
        );
        let root_graph = dependency_map(&app, "b.ts").expect("root graph");
        assert_eq!(root_graph.outgoing[0].target, MapTarget::File { path: "legacy/keep.ts".into() });
    }

    #[tokio::test]
    async fn ts_broken_config_keeps_bare_edges_unresolved_not_external() {
        let dir = tempfile::tempdir().expect("workspace");
        std::fs::write(
            dir.path().join("tsconfig.json"),
            "{\"compilerOptions\": {\"paths\": {\"@a/*\": [\"./src/*\"],,}}}}",
        ).expect("broken config");
        std::fs::create_dir_all(dir.path().join("src")).expect("src");
        std::fs::write(dir.path().join("src/b.ts"), "export {};\n").expect("b");
        std::fs::write(dir.path().join("a.ts"), "import '@a/x';\nimport './src/b';\n").expect("a");
        let (app, _agent) = application(dir.path()).await;
        let graph = dependency_map(&app, "a.ts").expect("graph");
        assert!(matches!(
            &graph.outgoing[0].target,
            MapTarget::Unresolved { reason } if reason.contains("tsconfig is unusable")
        ));
        assert_eq!(graph.outgoing[1].target, MapTarget::File { path: "src/b.ts".into() });
    }

    #[tokio::test]
    async fn ts_jsonc_tolerance_and_guards() {
        let build = |config: &str| -> tempfile::TempDir {
            let dir = tempfile::tempdir().expect("workspace");
            std::fs::write(dir.path().join("tsconfig.json"), config).expect("tsconfig");
            std::fs::create_dir_all(dir.path().join("src")).expect("src");
            std::fs::write(dir.path().join("src/x.ts"), "export {};\n").expect("x");
            std::fs::write(dir.path().join("a.ts"), "import '@a/x';\n").expect("a");
            dir
        };
        let tolerant = build(
            "{ /* c */ // line\n\"compilerOptions\": { \"paths\": { \"@a/*\": [\"./src/*\",], }, }, }",
        );
        let (app, _agent) = application(tolerant.path()).await;
        let graph = dependency_map(&app, "a.ts").expect("graph");
        assert_eq!(graph.outgoing[0].target, MapTarget::File { path: "src/x.ts".into() });

        let unclosed = build("/* never closed {\"compilerOptions\": {}}");
        let (app, _agent) = application(unclosed.path()).await;
        let graph = dependency_map(&app, "a.ts").expect("graph");
        assert!(matches!(
            &graph.outgoing[0].target,
            MapTarget::Unresolved { reason } if reason.contains("not closed")
        ));

        let merging = build("{\"compilerOptions\":{\"paths\":{\"@a/*\":[1/*c*/2]}}}");
        let (app, _agent) = application(merging.path()).await;
        let graph = dependency_map(&app, "a.ts").expect("graph");
        assert!(matches!(
            &graph.outgoing[0].target,
            MapTarget::Unresolved { reason } if reason.contains("tsconfig is unusable")
        ), "a token-merging comment must never fabricate the value 12");
    }

    #[tokio::test]
    async fn ts_extends_faults_stay_explicitly_unresolved() {
        let missing = {
            let dir = tempfile::tempdir().expect("workspace");
            std::fs::write(
                dir.path().join("tsconfig.json"),
                "{\"extends\":\"./missing.json\"}",
            ).expect("config");
            dir
        };
        let (app, _agent) = application(missing.path()).await;
        std::fs::write(missing.path().join("a.ts"), "import '@a/x';\n").expect("a");
        let graph = dependency_map(&app, "a.ts").expect("graph");
        assert!(matches!(
            &graph.outgoing[0].target,
            MapTarget::Unresolved { reason } if reason.contains("unusable")
        ));

        let cyclic = {
            let dir = tempfile::tempdir().expect("workspace");
            std::fs::write(dir.path().join("tsconfig.json"), "{\"extends\":\"./a.json\"}").expect("config");
            std::fs::write(dir.path().join("a.json"), "{\"extends\":\"./b.json\"}").expect("a");
            std::fs::write(dir.path().join("b.json"), "{\"extends\":\"./a.json\"}").expect("b");
            dir
        };
        let (app, _agent) = application(cyclic.path()).await;
        std::fs::write(cyclic.path().join("a.ts"), "import '@a/x';\n").expect("a");
        let graph = dependency_map(&app, "a.ts").expect("graph");
        assert!(matches!(
            &graph.outgoing[0].target,
            MapTarget::Unresolved { reason } if reason.contains("cyclic")
        ));

        let unsupported = {
            let dir = tempfile::tempdir().expect("workspace");
            std::fs::write(
                dir.path().join("tsconfig.json"),
                "{\"extends\":\"some-pkg/config\"}",
            ).expect("config");
            dir
        };
        let (app, _agent) = application(unsupported.path()).await;
        std::fs::write(unsupported.path().join("a.ts"), "import '@a/x';\n").expect("a");
        let graph = dependency_map(&app, "a.ts").expect("graph");
        assert!(matches!(
            &graph.outgoing[0].target,
            MapTarget::Unresolved { reason } if reason.contains("not a relative path")
        ));

        let deep = {
            let dir = tempfile::tempdir().expect("workspace");
            std::fs::write(dir.path().join("tsconfig.json"), "{\"extends\":\"./c0.json\"}").expect("config");
            for index in 0..10 {
                let parent = format!("./c{}.json", index + 1);
                std::fs::write(dir.path().join(format!("c{index}.json")), format!("{{\"extends\":\"{parent}\"}}"))
                    .expect("chain link");
            }
            dir
        };
        let (app, _agent) = application(deep.path()).await;
        std::fs::write(deep.path().join("a.ts"), "import '@a/x';\n").expect("a");
        let graph = dependency_map(&app, "a.ts").expect("graph");
        assert!(matches!(
            &graph.outgoing[0].target,
            MapTarget::Unresolved { reason } if reason.contains("exceeds 8")
        ));
    }

    #[tokio::test]
    async fn go_module_prefix_maps_root_and_package_directories() {
        let dir = tempfile::tempdir().expect("workspace");
        std::fs::write(dir.path().join("go.mod"), "module example.com/m\n\ngo 1.22\n").expect("go.mod");
        std::fs::write(
            dir.path().join("main.go"),
            "package main\n\nimport (\n\t\"example.com/m\"\n\t\"example.com/m/util\"\n\t\"example.com/m/missing\"\n\t\"github.com/other/x\"\n)\n",
        ).expect("main.go");
        std::fs::create_dir_all(dir.path().join("util")).expect("util dir");
        std::fs::write(dir.path().join("util/a.go"), "package util\n").expect("a.go");
        std::fs::write(dir.path().join("util/b.go"), "package util\n").expect("b.go");
        let (app, _agent) = application(dir.path()).await;
        let graph = dependency_map(&app, "main.go").expect("graph");
        assert_eq!(graph.outgoing.len(), 4);
        assert_eq!(graph.outgoing[0].target, MapTarget::Package { dir: ".".into() });
        assert_eq!(graph.outgoing[1].target, MapTarget::Package { dir: "util".into() });
        assert!(matches!(
            &graph.outgoing[2].target,
            MapTarget::Unresolved { reason } if reason.contains("does not exist")
        ), "a missing module package directory is never External");
        assert_eq!(graph.outgoing[3].target, MapTarget::External { specifier: "github.com/other/x".into() });
    }

    #[tokio::test]
    async fn go_package_proof_rejects_test_only_disagreement_and_missing_clause() {
        let dir = tempfile::tempdir().expect("workspace");
        std::fs::write(dir.path().join("go.mod"), "module example.com/m\n").expect("go.mod");
        std::fs::write(
            dir.path().join("main.go"),
            "package main\n\nimport (\n\t\"example.com/m/testonly\"\n\t\"example.com/m/mixed\"\n\t\"example.com/m/noclause\"\n)\n",
        ).expect("main.go");
        std::fs::create_dir_all(dir.path().join("testonly")).expect("dir");
        std::fs::write(dir.path().join("testonly/x_test.go"), "package testonly_test\n").expect("test file");
        std::fs::create_dir_all(dir.path().join("mixed")).expect("dir");
        std::fs::write(dir.path().join("mixed/a.go"), "package mixed\n").expect("a");
        std::fs::write(dir.path().join("mixed/b.go"), "package other\n").expect("b");
        std::fs::create_dir_all(dir.path().join("noclause")).expect("dir");
        std::fs::write(dir.path().join("noclause/a.go"), "func Early() {}\n").expect("no clause");
        let (app, _agent) = application(dir.path()).await;
        let graph = dependency_map(&app, "main.go").expect("graph");
        assert_eq!(graph.outgoing.len(), 3);
        assert!(matches!(
            &graph.outgoing[0].target,
            MapTarget::Unresolved { reason } if reason.contains("no non-test Go source")
        ));
        assert!(matches!(
            &graph.outgoing[1].target,
            MapTarget::Unresolved { reason } if reason.contains("disagree")
        ));
        assert!(matches!(
            &graph.outgoing[2].target,
            MapTarget::Unresolved { reason } if reason.contains("no package clause")
        ));
    }

    #[tokio::test]
    async fn go_without_module_or_with_broken_module_stays_explicit() {
        let plain = tempfile::tempdir().expect("workspace");
        std::fs::write(
            plain.path().join("a.go"),
            "package a\n\nimport (\n\t\"fmt\"\n\t\"C\"\n\t\"example.com/x/y\"\n)\n",
        ).expect("a.go");
        let (app, _agent) = application(plain.path()).await;
        let graph = dependency_map(&app, "a.go").expect("graph");
        assert_eq!(graph.outgoing.len(), 3);
        assert_eq!(graph.outgoing[0].target, MapTarget::External { specifier: "fmt".into() });
        assert_eq!(graph.outgoing[1].target, MapTarget::External { specifier: "C".into() });
        assert!(matches!(
            &graph.outgoing[2].target,
            MapTarget::Unresolved { reason } if reason.contains("no go.mod")
        ));

        let broken = tempfile::tempdir().expect("workspace");
        std::fs::write(broken.path().join("go.mod"), "module\n").expect("go.mod");
        std::fs::write(broken.path().join("b.go"), "package b\n\nimport \"fmt\"\n").expect("b.go");
        let (app, _agent) = application(broken.path()).await;
        let graph = dependency_map(&app, "b.go").expect("graph");
        assert!(matches!(
            &graph.outgoing[0].target,
            MapTarget::Unresolved { reason } if reason.contains("go.mod is unusable")
        ));
    }

    #[tokio::test]
    async fn rust_declared_mod_chain_resolves_files() {
        let dir = tempfile::tempdir().expect("workspace");
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        ).expect("manifest");
        std::fs::create_dir_all(dir.path().join("src/a")).expect("a dir");
        std::fs::create_dir_all(dir.path().join("src/outer")).expect("src tree");
        std::fs::write(dir.path().join("src/lib.rs"), "mod a;\nmod outer { mod inner; }\n").expect("lib");
        std::fs::write(dir.path().join("src/a.rs"), "mod b;\n").expect("a");
        std::fs::write(dir.path().join("src/a/b.rs"), "pub struct Leaf;\n").expect("b");
        std::fs::write(dir.path().join("src/outer/inner.rs"), "pub struct Inner;\n").expect("inner");
        let (app, _agent) = application(dir.path()).await;
        let generation = app.capture_generation();
        let selected = read_source(&generation, dir.path(), "src/lib.rs").expect("selected");
        let hop = one_hop_dependencies(&generation, dir.path(), selected).expect("hop");
        assert_eq!(hop.outgoing.len(), 2);
        assert_eq!(hop.outgoing[0].target, MapTarget::File { path: "src/a.rs".into() });
        assert_eq!(hop.outgoing[1].target, MapTarget::File { path: "src/outer/inner.rs".into() });
    }

    #[tokio::test]
    async fn rust_local_use_resolves_deepest_module_prefix() {
        let dir = tempfile::tempdir().expect("workspace");
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        ).expect("manifest");
        std::fs::create_dir_all(dir.path().join("src/a")).expect("src tree");
        std::fs::write(
            dir.path().join("src/lib.rs"),
            "mod a;\nuse crate::a::b::Leaf;\nuse crate::Nothing;\nuse std::io::Write;\n",
        ).expect("lib");
        std::fs::write(dir.path().join("src/a.rs"), "mod b;\n").expect("a");
        std::fs::write(dir.path().join("src/a/b.rs"), "pub struct Leaf;\n").expect("b");
        let (app, _agent) = application(dir.path()).await;
        let generation = app.capture_generation();
        let selected = read_source(&generation, dir.path(), "src/lib.rs").expect("selected");
        let hop = one_hop_dependencies(&generation, dir.path(), selected).expect("hop");
        assert_eq!(hop.outgoing.len(), 4);
        assert_eq!(hop.outgoing[0].target, MapTarget::File { path: "src/a.rs".into() });
        assert_eq!(
            hop.outgoing[1].target,
            MapTarget::File { path: "src/a/b.rs".into() },
            "the deepest proven module prefix file carries the symbol tail"
        );
        assert_eq!(
            hop.outgoing[2].target,
            MapTarget::File { path: "src/lib.rs".into() },
            "a zero-depth crate:: symbol lands on the crate root file"
        );
        assert_eq!(hop.outgoing[3].target, MapTarget::External { specifier: "std::io::Write".into() });
    }

    #[tokio::test]
    async fn rust_undeclared_source_stays_unresolved() {
        let dir = tempfile::tempdir().expect("workspace");
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        ).expect("manifest");
        std::fs::create_dir_all(dir.path().join("src")).expect("src");
        std::fs::write(dir.path().join("src/lib.rs"), "pub struct Keeper;\n").expect("lib");
        std::fs::write(dir.path().join("src/ghost.rs"), "use crate::Keeper;\n").expect("ghost");
        let (app, _agent) = application(dir.path()).await;
        let generation = app.capture_generation();
        let selected = read_source(&generation, dir.path(), "src/ghost.rs").expect("selected");
        let hop = one_hop_dependencies(&generation, dir.path(), selected).expect("hop");
        assert!(matches!(
            &hop.outgoing[0].target,
            MapTarget::Unresolved { reason } if reason.contains("does not reach the source")
        ));
    }

    #[tokio::test]
    async fn rust_symbol_bait_never_binds_same_named_file() {
        let dir = tempfile::tempdir().expect("workspace");
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        ).expect("manifest");
        std::fs::create_dir_all(dir.path().join("src")).expect("src");
        std::fs::write(dir.path().join("src/lib.rs"), "use crate::Thing;\n").expect("lib");
        std::fs::write(dir.path().join("src/Thing.rs"), "pub struct Thing;\n").expect("bait");
        let (app, _agent) = application(dir.path()).await;
        let generation = app.capture_generation();
        let selected = read_source(&generation, dir.path(), "src/lib.rs").expect("selected");
        let hop = one_hop_dependencies(&generation, dir.path(), selected).expect("hop");
        assert_eq!(hop.outgoing[0].target, MapTarget::File { path: "src/lib.rs".into() });
        assert!(!hop.outgoing.iter().any(|edge| {
            matches!(&edge.target, MapTarget::File { path } if path == "src/Thing.rs")
        }), "an undeclared same-named file is never a module");
    }

    #[tokio::test]
    async fn rust_conservative_and_path_attributes_block_resolution() {
        let dir = tempfile::tempdir().expect("workspace");
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        ).expect("manifest");
        std::fs::create_dir_all(dir.path().join("src")).expect("src");
        std::fs::write(
            dir.path().join("src/lib.rs"),
            "#[cfg(test)]\nmod tests;\n#[path = \"alt.rs\"]\nmod weird;\nmod plain;\n",
        ).expect("lib");
        std::fs::write(dir.path().join("src/tests.rs"), "pub struct Tests;\n").expect("tests");
        std::fs::write(dir.path().join("src/alt.rs"), "pub struct Weird;\n").expect("alt");
        std::fs::write(dir.path().join("src/plain.rs"), "pub struct Plain;\n").expect("plain");
        let (app, _agent) = application(dir.path()).await;
        let generation = app.capture_generation();
        let selected = read_source(&generation, dir.path(), "src/lib.rs").expect("selected");
        let hop = one_hop_dependencies(&generation, dir.path(), selected).expect("hop");
        assert_eq!(hop.outgoing.len(), 3);
        assert!(matches!(
            &hop.outgoing[0].target,
            MapTarget::Unresolved { reason } if reason.contains("conservative attribute")
        ));
        assert!(matches!(
            &hop.outgoing[1].target,
            MapTarget::Unresolved { reason } if reason.contains("#[path]")
        ));
        assert_eq!(hop.outgoing[2].target, MapTarget::File { path: "src/plain.rs".into() });
    }

    #[tokio::test]
    async fn rust_ambiguous_module_files_stay_unresolved() {
        let dir = tempfile::tempdir().expect("workspace");
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        ).expect("manifest");
        std::fs::create_dir_all(dir.path().join("src/both")).expect("both dir");
        std::fs::write(dir.path().join("src/lib.rs"), "mod both;\n").expect("lib");
        std::fs::write(dir.path().join("src/both.rs"), "pub struct Both;\n").expect("flat");
        std::fs::write(dir.path().join("src/both/mod.rs"), "pub struct Both;\n").expect("directory");
        let (app, _agent) = application(dir.path()).await;
        let generation = app.capture_generation();
        let selected = read_source(&generation, dir.path(), "src/lib.rs").expect("selected");
        let hop = one_hop_dependencies(&generation, dir.path(), selected).expect("hop");
        assert!(matches!(
            &hop.outgoing[0].target,
            MapTarget::Unresolved { reason } if reason.contains("module file candidates are ambiguous")
        ));
    }

    #[tokio::test]
    async fn rust_explicit_bin_target_resolves_and_autobins_off_stays_unresolved() {
        let explicit = tempfile::tempdir().expect("workspace");
        std::fs::write(
            explicit.path().join("Cargo.toml"),
            "[[bin]]\nname = \"tool\"\npath = \"tool/main.rs\"\n",
        ).expect("manifest");
        std::fs::create_dir_all(explicit.path().join("tool")).expect("tool dir");
        std::fs::write(explicit.path().join("tool/main.rs"), "mod sub;\n").expect("bin root");
        std::fs::write(explicit.path().join("tool/sub.rs"), "pub struct Sub;\n").expect("sub");
        let (app, _agent) = application(explicit.path()).await;
        let generation = app.capture_generation();
        let selected = read_source(&generation, explicit.path(), "tool/main.rs").expect("selected");
        let hop = one_hop_dependencies(&generation, explicit.path(), selected).expect("hop");
        assert_eq!(hop.outgoing[0].target, MapTarget::File { path: "tool/sub.rs".into() });

        let disabled = tempfile::tempdir().expect("workspace");
        std::fs::write(
            disabled.path().join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nautobins = false\n",
        ).expect("manifest");
        std::fs::create_dir_all(disabled.path().join("src/bin")).expect("bin dir");
        std::fs::write(disabled.path().join("src/bin/z.rs"), "use crate::Nothing;\n").expect("z");
        let (app, _agent) = application(disabled.path()).await;
        let generation = app.capture_generation();
        let selected = read_source(&generation, disabled.path(), "src/bin/z.rs").expect("selected");
        let hop = one_hop_dependencies(&generation, disabled.path(), selected).expect("hop");
        assert!(matches!(
            &hop.outgoing[0].target,
            MapTarget::Unresolved { reason } if reason.contains("no cargo target module chain reaches the source")
        ));
    }

    #[tokio::test]
    async fn rust_super_and_inline_use_scopes_walk_declared_modules() {
        let dir = tempfile::tempdir().expect("workspace");
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        ).expect("manifest");
        std::fs::create_dir_all(dir.path().join("src/a")).expect("src tree");
        std::fs::write(dir.path().join("src/lib.rs"), "mod a;\nmod root_thing {}\n").expect("lib");
        std::fs::write(dir.path().join("src/a.rs"), "mod inner { use super::helper; }\nmod helper;\nmod b;\n").expect("a");
        std::fs::write(dir.path().join("src/a/helper.rs"), "pub struct Helper;\n").expect("helper");
        std::fs::write(dir.path().join("src/a/b.rs"), "use super::super::root_thing;\n").expect("b");
        let (app, _agent) = application(dir.path()).await;
        let generation = app.capture_generation();
        let selected = read_source(&generation, dir.path(), "src/a.rs").expect("selected");
        let hop = one_hop_dependencies(&generation, dir.path(), selected).expect("hop");
        assert_eq!(hop.outgoing.len(), 3);
        assert_eq!(hop.outgoing[0].target, MapTarget::File { path: "src/a/helper.rs".into() });
        assert_eq!(hop.outgoing[1].target, MapTarget::File { path: "src/a/helper.rs".into() });
        assert_eq!(hop.outgoing[2].target, MapTarget::File { path: "src/a/b.rs".into() });
        let selected = read_source(&generation, dir.path(), "src/a/b.rs").expect("selected");
        let hop = one_hop_dependencies(&generation, dir.path(), selected).expect("hop");
        assert_eq!(hop.outgoing[0].target, MapTarget::File { path: "src/lib.rs".into() });
    }

    /// A config-relative join that underflows the workspace root or starts
    /// with an absolute path can never name a workspace-relative file; the
    /// old flattening turned `../../outside/x` from the root config into
    /// `outside/x` and bound a same-named workspace decoy.
    #[test]
    fn join_config_path_rejects_underflow_and_absolute() {
        assert_eq!(
            join_config_path("web/src", "../app/x.ts"),
            Some("web/app/x.ts".into())
        );
        assert_eq!(join_config_path("web/src", "./keep.ts"), Some("web/src/keep.ts".into()));
        assert_eq!(join_config_path("web", "../../outside/x"), None, "underflow past the root");
        assert_eq!(join_config_path("", "../outside/x"), None, "underflow from the root");
        assert_eq!(join_config_path("web", "/etc/x"), None, "absolute paths stay absolute");
        assert_eq!(join_config_path("", "src/x.ts"), Some("src/x.ts".into()));
    }

    /// Alias templates that escape the workspace — by root underflow or by
    /// absolute path — are not resolvable alternatives. They must never be
    /// flattened into a safe-looking relative path and bind a same-named
    /// workspace decoy.
    #[tokio::test]
    async fn ts_escaping_alias_targets_never_bind_workspace_decoys() {
        let dir = tempfile::tempdir().expect("workspace");
        std::fs::write(
            dir.path().join("tsconfig.json"),
            "{\"compilerOptions\":{\"paths\":{\"@up/*\":[\"../../outside/*\"],\"@abs/*\":[\"/outside/*\"],\"@ok/*\":[\"./src/*\"]}}}",
        ).expect("tsconfig");
        std::fs::create_dir_all(dir.path().join("src")).expect("src");
        std::fs::create_dir_all(dir.path().join("outside")).expect("decoy tree");
        std::fs::write(dir.path().join("outside/x.ts"), "export const decoy = 1;\n").expect("decoy");
        std::fs::write(dir.path().join("src/x.ts"), "export const real = 1;\n").expect("real");
        std::fs::write(dir.path().join("a.ts"), "import '@up/x';\nimport '@abs/y';\nimport '@ok/x';\n").expect("a");
        let (app, _agent) = application(dir.path()).await;
        let graph = dependency_map(&app, "a.ts").expect("graph");
        assert_eq!(graph.outgoing.len(), 3);
        assert_eq!(
            graph.outgoing[0].target,
            MapTarget::Unresolved { reason: "no tsconfig paths candidate exists".into() },
            "the underflowing template is skipped, never flattened onto outside/x.ts"
        );
        assert_eq!(
            graph.outgoing[1].target,
            MapTarget::Unresolved { reason: "no tsconfig paths candidate exists".into() },
            "the absolute template is skipped, never flattened onto outside/x.ts"
        );
        assert_eq!(graph.outgoing[2].target, MapTarget::File { path: "src/x.ts".into() });
        assert!(graph.outgoing.iter().all(|edge| {
            !matches!(&edge.target, MapTarget::File { path } if path == "outside/x.ts")
        }));
    }

    /// A deep source beneath a `mod.rs` intermediate is proven through the
    /// declared steps only: lib.rs declares `a`, `a/mod.rs` declares `b`, and
    /// only then does `a/b.rs` belong to the module tree.
    #[tokio::test]
    async fn rust_mod_rs_intermediate_chain_proves_deep_source() {
        let dir = tempfile::tempdir().expect("workspace");
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        ).expect("manifest");
        std::fs::create_dir_all(dir.path().join("src/a")).expect("a dir");
        std::fs::write(dir.path().join("src/lib.rs"), "mod a;\n").expect("lib");
        std::fs::write(dir.path().join("src/a/mod.rs"), "mod b;\n").expect("a");
        std::fs::write(dir.path().join("src/a/b.rs"), "use crate::Nothing;\npub struct Leaf;\n").expect("b");
        let (app, _agent) = application(dir.path()).await;
        let generation = app.capture_generation();
        let selected = read_source(&generation, dir.path(), "src/a/b.rs").expect("selected");
        let hop = one_hop_dependencies(&generation, dir.path(), selected).expect("hop");
        assert_eq!(hop.outgoing.len(), 1);
        assert_eq!(
            hop.outgoing[0].target,
            MapTarget::File { path: "src/lib.rs".into() },
            "the crate:: prefix requires the proven lib → a/mod.rs → a/b.rs chain"
        );
        let selected = read_source(&generation, dir.path(), "src/a/mod.rs").expect("selected");
        let hop = one_hop_dependencies(&generation, dir.path(), selected).expect("hop");
        assert_eq!(hop.outgoing[0].target, MapTarget::File { path: "src/a/b.rs".into() });
    }

    /// A baseUrl escaping the workspace is an explicit unresolved, never the
    /// External fallback and never a flattened workspace decoy.
    #[tokio::test]
    async fn ts_escaping_base_url_stays_unresolved_not_external() {
        let dir = tempfile::tempdir().expect("workspace");
        std::fs::write(
            dir.path().join("tsconfig.json"),
            "{\"compilerOptions\":{\"baseUrl\":\"../../outside\"}}",
        ).expect("tsconfig");
        std::fs::create_dir_all(dir.path().join("outside")).expect("decoy tree");
        std::fs::write(dir.path().join("outside/x.ts"), "export const decoy = 1;\n").expect("decoy");
        std::fs::write(dir.path().join("a.ts"), "import 'x';\n").expect("a");
        let (app, _agent) = application(dir.path()).await;
        let graph = dependency_map(&app, "a.ts").expect("graph");
        assert!(matches!(
            &graph.outgoing[0].target,
            MapTarget::Unresolved { reason } if reason.contains("escapes the workspace")
        ), "the escaping baseUrl is explicit, never External and never the decoy");
    }

    /// A listed but unreadable package member leaves the member set
    /// incomplete: the import stays explicitly unresolved.
    #[tokio::test]
    async fn go_unreadable_member_keeps_package_unresolved() {
        let dir = tempfile::tempdir().expect("workspace");
        std::fs::write(dir.path().join("go.mod"), "module example.com/m\n").expect("go.mod");
        std::fs::write(dir.path().join("main.go"), "package main\n\nimport \"example.com/m/g\"\n").expect("main.go");
        std::fs::create_dir_all(dir.path().join("g")).expect("g dir");
        std::fs::write(dir.path().join("g/huge.go"), vec![b' '; MAX_SOURCE_BYTES + 1]).expect("member");
        let (app, _agent) = application(dir.path()).await;
        let graph = dependency_map(&app, "main.go").expect("graph");
        assert!(matches!(
            &graph.outgoing[0].target,
            MapTarget::Unresolved { reason } if reason.contains("membership is not provable")
        ), "an unreadable member can never prove the package");
    }

    /// The selected Go source is proven from its single captured read: the
    /// live bytes are rewritten to a disagreeing package clause after the
    /// capture, so any second read of the selected would fail the proof.
    #[tokio::test]
    async fn go_selected_package_proof_never_rereads_the_capture() {
        let dir = tempfile::tempdir().expect("workspace");
        std::fs::write(dir.path().join("go.mod"), "module example.com/m\n").expect("go.mod");
        std::fs::write(dir.path().join("aux.go"), "package main\n").expect("aux");
        std::fs::write(dir.path().join("main.go"), "package main\n\nimport \"example.com/m\"\n").expect("main.go");
        let (app, _agent) = application(dir.path()).await;
        let generation = app.capture_generation();
        let selected = read_source(&generation, dir.path(), "main.go").expect("selected");
        std::fs::write(dir.path().join("main.go"), "package other\n\nimport \"example.com/m\"\n").expect("rewrite");
        let hop = one_hop_dependencies(&generation, dir.path(), selected).expect("hop");
        assert_eq!(hop.outgoing.len(), 1);
        assert_eq!(
            hop.outgoing[0].target,
            MapTarget::Package { dir: ".".into() },
            "the selected member's clause comes from the single capture, never a re-read"
        );
    }

    /// At a nonstandard crate root, popping an inline module must compute
    /// the enclosing scope from the crate root's own directory, not from
    /// the root file's stem directory.
    #[tokio::test]
    async fn rust_inline_pop_uses_crate_scope_at_nonstandard_root() {
        let dir = tempfile::tempdir().expect("workspace");
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[[bin]]\nname = \"tool\"\npath = \"tool/main.rs\"\n",
        ).expect("manifest");
        std::fs::create_dir_all(dir.path().join("tool")).expect("tool dir");
        std::fs::write(
            dir.path().join("tool/main.rs"),
            "mod inner { use super::thing; }\nmod thing;\n",
        ).expect("bin root");
        std::fs::write(dir.path().join("tool/thing.rs"), "pub struct Thing;\n").expect("thing");
        let (app, _agent) = application(dir.path()).await;
        let generation = app.capture_generation();
        let selected = read_source(&generation, dir.path(), "tool/main.rs").expect("selected");
        let hop = one_hop_dependencies(&generation, dir.path(), selected).expect("hop");
        assert_eq!(hop.outgoing.len(), 2);
        assert_eq!(hop.outgoing[0].target, MapTarget::File { path: "tool/thing.rs".into() });
        assert_eq!(hop.outgoing[1].target, MapTarget::File { path: "tool/thing.rs".into() });
    }

    /// A declared module whose conventional file is missing resolves as an
    /// explicit missing-module failure, while an undeclared tail stays a
    /// symbol on the proven scope file.
    #[tokio::test]
    async fn rust_declared_module_without_file_stays_unresolved() {
        let dir = tempfile::tempdir().expect("workspace");
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        ).expect("manifest");
        std::fs::create_dir_all(dir.path().join("src")).expect("src");
        std::fs::write(dir.path().join("src/lib.rs"), "mod a;\nuse crate::a::Tail;\n").expect("lib");
        let (app, _agent) = application(dir.path()).await;
        let generation = app.capture_generation();
        let selected = read_source(&generation, dir.path(), "src/lib.rs").expect("selected");
        let hop = one_hop_dependencies(&generation, dir.path(), selected).expect("hop");
        assert_eq!(hop.outgoing.len(), 2);
        assert!(matches!(&hop.outgoing[0].target, MapTarget::Unresolved { .. }));
        assert!(matches!(
            &hop.outgoing[1].target,
            MapTarget::Unresolved { reason } if reason.contains("has no module file")
        ));
    }

    /// A `use` inside a controlled inline ancestor must stay explicitly
    /// unresolved exactly like the `mod` path: cfg/cfg_attr/path ancestors
    /// make the directory convention unusable; the unattributed control
    /// still resolves to the declared file.
    #[tokio::test]
    async fn rust_controlled_inline_ancestor_keeps_use_unresolved() {
        let cases: &[(&str, &str, &str)] = &[
            ("cfg", "#[cfg(test)]\n", "conservative attribute"),
            ("cfg_attr", "#[cfg_attr(feature = \"x\", cfg(test))]\n", "conservative attribute"),
            ("path", "#[path = \"alt\"]\n", "#[path]"),
            ("control", "", ""),
        ];
        for (name, attribute, reason) in cases {
            let dir = tempfile::tempdir().expect("workspace");
            std::fs::write(
                dir.path().join("Cargo.toml"),
                "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
            ).expect("manifest");
            std::fs::create_dir_all(dir.path().join("src")).expect("src");
            std::fs::write(
                dir.path().join("src/lib.rs"),
                format!("{attribute}mod inner {{ use crate::plain; }}\nmod plain;\n"),
            ).expect("lib");
            std::fs::write(dir.path().join("src/plain.rs"), "pub struct Plain;\n").expect("plain");
            let (app, _agent) = application(dir.path()).await;
            let generation = app.capture_generation();
            let selected = read_source(&generation, dir.path(), "src/lib.rs").expect("selected");
            let hop = one_hop_dependencies(&generation, dir.path(), selected).expect("hop");
            // The top-level `mod plain;` edge is incidental here; only the
            // use edge inside the inline ancestor is under audit.
            let use_edge = hop
                .outgoing
                .iter()
                .find(|edge| edge.kind == EdgeKind::Use && edge.specifier == "crate::plain")
                .unwrap_or_else(|| panic!("{name}: the crate::plain use edge is missing"));
            assert_eq!(use_edge.source, "src/lib.rs", "{name}");
            if *name == "control" {
                assert_eq!(
                    use_edge.target,
                    MapTarget::File { path: "src/plain.rs".into() },
                    "{name}: the unattributed inline ancestor resolves normally"
                );
            } else {
                assert!(
                    matches!(&use_edge.target,
                        MapTarget::Unresolved { reason: got } if got.contains(reason)),
                    "{name}: the controlled inline ancestor must report {reason}, never a file"
                );
            }
        }
    }
}

fn allowed_source_omission(error: &anyhow::Error) -> bool {
    matches!(error.downcast_ref::<FsError>(),
        Some(FsError::ReadLimitExceeded { .. } | FsError::UnsupportedEntryType))
        || matches!(error.downcast_ref::<FsError>(),
            Some(FsError::System { kind: std::io::ErrorKind::NotFound, .. }))
        || error.downcast_ref::<std::str::Utf8Error>().is_some()
}
