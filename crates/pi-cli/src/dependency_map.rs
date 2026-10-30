//! Authorized, bounded one-hop dependency maps. Purpose text is verbatim source
//! documentation, never a generated description. Resolution is deliberately
//! conservative: unsupported Rust contexts remain unresolved rather than guessed.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use pi_coding::{Application, FsError};
use pi_coding::application::ApplicationGeneration;
use pi_coding::dependency_map::{DeclarationEntry, DependencyEdge, EdgeKind, MapLanguage, ParseStatus, SourceDependencies, extract_dependencies};
use serde::Serialize;

use crate::file_explorer;

pub const MAX_SCAN_FILES: usize = 5_000;
pub const MAX_SOURCE_BYTES: usize = 1024 * 1024;

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
    let (_, path) = file_explorer::resolve_scoped_path(root, path, false)?;
    let selected = read_source(&generation, root, &path)?;
    let mut sources = BTreeMap::new();
    let mut coverage = MapCoverage { visible_entries_only: true, incomplete: true, ..Default::default() };
    record_source(&mut coverage, &selected);
    sources.insert(path.clone(), selected);
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
                Ok(source) => {
                    record_source(&mut coverage, &source);
                    sources.insert(entry.path, source);
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
    let mut response = assemble(&path, &sources, coverage, &|candidate| candidate_exists(&generation, root, candidate))?;
    // This is a payload bound, not an evidence truncation policy.
    response.nodes.sort_by(|a, b| a.path.cmp(&b.path));
    if serde_json::to_vec(&response)?.len() > 256 * 1024 {
        return Err(anyhow!("dependency map exceeds response limit"));
    }
    Ok(response)
}

fn read_source(generation: &ApplicationGeneration, root: &Path, path: &str) -> Result<SourceDependencies> {
    let (absolute, relative) = file_explorer::resolve_scoped_path(root, path, false)?;
    let bytes = generation.read_file_bounded(&absolute, MAX_SOURCE_BYTES as u64)
        .with_context(|| format!("dependency source unavailable: {relative}"))?;
    let text = std::str::from_utf8(&bytes)
        .with_context(|| format!("dependency source is not UTF-8: {relative}"))?;
    Ok(extract_dependencies(&relative, text))
}

fn record_source(coverage: &mut MapCoverage, source: &SourceDependencies) {
    coverage.scanned_files += 1;
    coverage.unsupported_files += usize::from(source.parse == ParseStatus::Unsupported);
    coverage.incomplete_files += usize::from(source.parse == ParseStatus::Incomplete);
    coverage.dynamic_omitted += source.dynamic_omitted;
}

fn assemble(path: &str, sources: &BTreeMap<String, SourceDependencies>, coverage: MapCoverage, exists: &impl Fn(&str) -> Result<bool>) -> Result<DependencyMapResponse> {
    let mut outgoing = Vec::new();
    let mut incoming = Vec::new();
    let mut nodes = BTreeSet::from([path.to_owned()]);
    for (source_path, source) in sources {
        for evidence in &source.edges {
            let target = resolve_edge(source_path, evidence, sources, exists)?;
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

fn unresolved(reason: &str) -> MapTarget {
    MapTarget::Unresolved { reason: reason.to_owned() }
}

fn resolve_edge(source: &str, edge: &DependencyEdge, sources: &BTreeMap<String, SourceDependencies>, exists: &impl Fn(&str) -> Result<bool>) -> Result<MapTarget> {
    if matches!(edge.kind, EdgeKind::Use | EdgeKind::Mod) {
        // The pure extractor deliberately does not carry lexical module ancestry,
        // #[path], cfg, crate target roots, or symbol bindings. Even top-level
        // `mod` can have #[path]; filename heuristics would invent relations.
        return Ok(unresolved("Rust module ancestry, attributes and crate bindings are not available"));
    }
    let specifier = &edge.specifier;
    let relative = specifier.starts_with("./") || specifier.starts_with("../");
    if !relative && edge.kind != EdgeKind::SolImport {
        return Ok(if specifier.starts_with('/') {
            unresolved("absolute imports are outside relative resolution")
        } else {
            MapTarget::External { specifier: specifier.clone() }
        });
    }
    if specifier.starts_with('/') || specifier.contains('\\') {
        return Ok(unresolved("absolute or platform-specific import is not supported"));
    }
    // Solidity non-relative imports are exact source-unit paths from the
    // workspace root. Remappings/package resolution is never guessed.
    let joined = if relative {
        Path::new(source).parent().unwrap_or(Path::new("")).join(specifier)
    } else {
        Path::new(specifier).to_path_buf()
    };
    let Ok((_, base)) = file_explorer::resolve_scoped_path(Path::new("/"), &joined.to_string_lossy(), false) else {
        return Ok(unresolved("relative import escapes workspace"));
    };
    let mut candidates = BTreeSet::from([base.clone()]);
    if edge.kind != EdgeKind::SolImport && Path::new(&base).extension().is_none() {
        for suffix in [".ts", ".tsx", ".js", ".jsx", ".mts", ".cts", ".mjs", ".cjs"] {
            candidates.insert(format!("{base}{suffix}"));
            candidates.insert(format!("{base}/index{suffix}"));
        }
    }
    let mut matches = Vec::new();
    for candidate in candidates {
        match exists(&candidate) {
            Ok(true) => matches.push(candidate),
            Ok(false) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(match matches.as_slice() {
        [path] if sources.contains_key(path) => MapTarget::File { path: path.clone() },
        [] => unresolved("no relative file candidate"),
        [_] => unresolved("target was not readable within the bounded source scan"),
        _ => unresolved("multiple relative file candidates"),
    })
}

fn candidate_exists(generation: &ApplicationGeneration, root: &Path, candidate: &str) -> Result<bool> {
    let (absolute, _) = file_explorer::resolve_scoped_path(root, candidate, false)?;
    match generation.read_file_bounded(&absolute, 1) {
        Ok(_) => Ok(true),
        Err(error) => match error.downcast_ref::<FsError>() {
            Some(FsError::ReadLimitExceeded { .. }) => Ok(true),
            Some(FsError::UnsupportedEntryType) => Ok(false),
            Some(FsError::System { kind, .. }) if matches!(kind, std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory) => Ok(false),
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
                model, cwd: cwd.to_path_buf(), system_prompt: String::new(),
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
        std::fs::write(dir.path().join("a.ts"), "import './link'; import './linked/secret';").expect("a");
        std::os::unix::fs::symlink(outside.path().join("secret.ts"), dir.path().join("link.ts")).expect("file link");
        std::os::unix::fs::symlink(outside.path(), dir.path().join("linked")).expect("directory link");
        let (app, _agent) = application(dir.path()).await;
        assert!(dependency_map(&app, "link.ts").is_err());
        assert!(dependency_map(&app, "linked/secret.ts").is_err());
        assert!(dependency_map(&app, "a.ts").is_err(), "symlink authority errors abort rather than disappear");
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

    #[test]
    fn rust_context_is_explicitly_unresolved_not_filename_guessed() {
        let sources = BTreeMap::from([
            ("src/lib.rs".into(), extract_dependencies("src/lib.rs", "mod nested { mod child; use super::Thing; }")),
            ("src/child.rs".into(), extract_dependencies("src/child.rs", "pub struct Thing;")),
        ]);
        let graph = assemble("src/lib.rs", &sources, MapCoverage::default(), &|path| Ok(sources.contains_key(path))).expect("graph");
        assert_eq!(graph.outgoing.len(), 2);
        assert!(graph.outgoing.iter().all(|edge| matches!(edge.target, MapTarget::Unresolved { .. })));
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

    #[test]
    fn unexpected_probe_errors_propagate_and_authority_errors_are_not_omissions() {
        let sources = BTreeMap::from([("a.ts".into(), extract_dependencies("a.ts", "import './b';"))]);
        let error = assemble("a.ts", &sources, MapCoverage::default(), &|_| Err(FsError::Denied.into())).expect_err("denied probe");
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
        let source = read_source(&retained, dir.path(), "a.ts").expect("retained source");
        assert_eq!(source.edges.len(), 1);
        assert!(candidate_exists(&retained, dir.path(), "b.ts").expect("retained probe"));
        let listing = file_explorer::list_with_generation(&retained, dir.path(), "").expect("retained listing");
        assert!(listing.entries.iter().any(|entry| entry.path == "a.ts"));
    }
}

fn allowed_source_omission(error: &anyhow::Error) -> bool {
    matches!(error.downcast_ref::<FsError>(),
        Some(FsError::ReadLimitExceeded { .. } | FsError::UnsupportedEntryType))
        || matches!(error.downcast_ref::<FsError>(),
            Some(FsError::System { kind: std::io::ErrorKind::NotFound, .. }))
        || error.downcast_ref::<std::str::Utf8Error>().is_some()
}
