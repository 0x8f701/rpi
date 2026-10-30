//! Product-surface adapter for the unified native and foreign session catalog.
//!
//! This module deliberately contains no terminal rendering. The line REPL,
//! startup argument handling, and a later TUI integration all consume the same
//! request/row/selection types and selection semantics.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use pi_coding::{
    Application, CatalogError, CatalogListOptions, CatalogRow, CatalogRowStatus,
    CatalogSearchOptions, CatalogSort, ResolvedSession, SessionCatalog, SessionKind,
    SessionSourceKind,
};

/// Query used to populate a unified resume selector.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResumeCatalogRequest {
    /// Empty or absent means an unfiltered list.
    pub query: Option<String>,
    /// Sources eligible for automatic discovery. Native Pi is always added;
    /// empty therefore means native-only.
    pub sources: Vec<SessionSourceKind>,
    pub include_foreign: bool,
    pub dedupe: bool,
    pub named_only: bool,
    pub cwd_scope: Option<PathBuf>,
    pub sort: CatalogSort,
}

impl Default for ResumeCatalogRequest {
    fn default() -> Self {
        Self {
            query: None,
            sources: vec![SessionSourceKind::NativePi],
            include_foreign: true,
            dedupe: true,
            named_only: false,
            cwd_scope: None,
            sort: CatalogSort::Newest,
        }
    }
}
/// Normalize an automatic-resume policy. Native Pi is always available and an
/// empty configured allowlist never expands to every catalog source.
#[must_use]
pub fn normalize_resume_sources(sources: &[SessionSourceKind]) -> Vec<SessionSourceKind> {
    let mut effective = Vec::with_capacity(sources.len() + 1);
    effective.push(SessionSourceKind::NativePi);
    for source in sources {
        if !effective.contains(source) {
            effective.push(*source);
        }
    }
    effective
}

/// Read the effective automatic-resume policy attached to the live application.
/// Applications without resources use the product default: native Pi only.
#[must_use]
pub fn effective_resume_sources(application: &Application) -> Vec<SessionSourceKind> {
    application.resource_snapshot().map_or_else(
        || pi_coding::Settings::default().effective_session_import_sources(),
        |snapshot| snapshot.settings.effective_session_import_sources(),
    )
}

const WEB_DEFAULT_RESUME_SOURCES: [SessionSourceKind; 5] = [
    SessionSourceKind::NativePi,
    SessionSourceKind::Pi,
    SessionSourceKind::Omp,
    SessionSourceKind::Codex,
    SessionSourceKind::Grok,
];

fn web_resume_sources_from_settings(settings: &pi_coding::Settings) -> Vec<SessionSourceKind> {
    if settings.session_import_sources.is_none() {
        return WEB_DEFAULT_RESUME_SOURCES.to_vec();
    }
    settings.effective_session_import_sources()
}

/// Read the Web listener's session discovery policy. An absent setting enables
/// the common local coding-agent stores; an explicit setting remains authoritative.
#[must_use]
pub fn web_resume_sources(application: &Application) -> Vec<SessionSourceKind> {
    application.resource_snapshot().map_or_else(
        || web_resume_sources_from_settings(&pi_coding::Settings::default()),
        |snapshot| web_resume_sources_from_settings(&snapshot.settings),
    )
}

/// Rendering-neutral row exposed to resume selectors.
#[derive(Clone, Debug, PartialEq)]
pub struct ResumeSelectorRow {
    pub source: SessionSourceKind,
    pub source_badge: &'static str,
    pub session_id: String,
    pub cwd: PathBuf,
    pub modified_epoch: f64,
    pub display_time: String,
    pub summary: String,
    pub path: PathBuf,
    pub size: u64,
    pub message_count: Option<usize>,
    pub name: Option<String>,
    pub status: CatalogRowStatus,
    /// Catalog-built corpus for local fuzzy filtering without rescanning files.
    pub search_text: String,
    /// Isolated message corpus matched by the selector without crossing into
    /// cwd/path; `search_text` remains for stable identity ordering.
    pub message_blob: String,
    pub has_parent_session: bool,
    /// Authoritative header provenance; workflow-internal rows are already
    /// excluded upstream by the catalog but the flag stays consumable.
    pub session_kind: SessionKind,
    /// True when the session's cwd sits under a `.slock/agents/<id>` Slock
    /// ephemeral agent workspace (Codex/Slock orchestration, not a user
    /// project). View signal for selector grouping; never a filter.
    pub agent_session: bool,
    /// True when the session carries parsed messages but none contain
    /// genuine content (bootstrap-only: environment context, user info,
    /// system reminders, AGENTS/project instructions). View signal only.
    pub bootstrap_only: bool,
}

impl From<CatalogRow> for ResumeSelectorRow {
    fn from(row: CatalogRow) -> Self {
        Self {
            source: row.kind,
            source_badge: row.kind.label(),
            session_id: row.session_id,
            cwd: row.cwd,
            modified_epoch: row.modified_epoch,
            display_time: row.display_time,
            summary: row.summary,
            path: row.path,
            size: row.size,
            message_count: row.message_count,
            name: row.name,
            status: row.status,
            search_text: row.search_text,
            message_blob: row.message_blob,
            has_parent_session: row.has_parent_session,
            session_kind: row.session_kind,
            agent_session: row.agent_session,
            bootstrap_only: row.bootstrap_only,
        }
    }
}

/// Result returned to a selector implementation.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ResumeCatalogResult {
    pub rows: Vec<ResumeSelectorRow>,
}

/// Stable selection payload. It contains enough information to avoid reading
/// or re-importing a row that the catalog already marked as native/imported.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResumeSelectorTarget {
    pub source: SessionSourceKind,
    pub source_path: PathBuf,
    pub status: CatalogRowStatus,
}

impl From<&ResumeSelectorRow> for ResumeSelectorTarget {
    fn from(row: &ResumeSelectorRow) -> Self {
        Self {
            source: row.source,
            source_path: row.path.clone(),
            status: row.status.clone(),
        }
    }
}

/// Selection may come from a catalog row or a path/id typed by the user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResumeSelectionRequest {
    Target(ResumeSelectorTarget),
    Input(String),
}

/// Native path ready for [`Application::switch_session`], plus observable
/// provenance for status messages and tests.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResumeSelectionResult {
    pub source: SessionSourceKind,
    pub path: PathBuf,
    pub imported: bool,
    pub reused_existing: bool,
    pub native_no_copy: bool,
    pub message_count: Option<usize>,
    pub cancelled: bool,
    /// User-facing notice carried through from `SessionChangeOutcome::notice`
    /// when a same-CWD resume completed with orchestration left unbound after
    /// a durable policy mismatch. `None` on a normal resume/cancellation.
    pub notice: Option<String>,
}

/// List/search/sort the unified catalog for a selector.
pub fn load_resume_catalog(
    catalog: &SessionCatalog,
    request: &ResumeCatalogRequest,
) -> Result<ResumeCatalogResult, CatalogError> {
    let sources = normalize_resume_sources(&request.sources);
    let rows = match request
        .query
        .as_deref()
        .map(str::trim)
        .filter(|query| !query.is_empty())
    {
        Some(query) => catalog.search(
            query,
            &CatalogSearchOptions {
                sources,
                include_foreign: request.include_foreign,
                dedupe: request.dedupe,
                named_only: request.named_only,
                cwd_scope: request.cwd_scope.clone(),
            },
        )?,
        None => catalog.list(&CatalogListOptions {
            sources,
            include_foreign: request.include_foreign,
            dedupe: request.dedupe,
            named_only: request.named_only,
            cwd_scope: request.cwd_scope.clone(),
        }),
    };
    Ok(ResumeCatalogResult {
        rows: SessionCatalog::sort_rows(rows, request.sort)
            .into_iter()
            .map(ResumeSelectorRow::from)
            .collect(),
    })
}


const WEB_NOISE_SESSION_MAX_BYTES: u64 = 10 * 1024;

/// Hide small persisted rows that contain no user/assistant messages from the
/// Web catalog, except native child journals whose retained header proves a
/// `parentSession` lineage. Root forks intentionally exclude the selected
/// prompt and may otherwise be header-only after recreation.
/// Temp-workspace rows are NOT dropped here: they are flagged `temporary` by
/// [`partition_web_noise_rows`] so the sidebar can hide them by default.
#[must_use]
pub fn filter_web_noise_rows(rows: Vec<ResumeSelectorRow>) -> Vec<ResumeSelectorRow> {
    rows.into_iter()
        .filter(|row| {
            row.size >= WEB_NOISE_SESSION_MAX_BYTES
                || row.message_count != Some(0)
                || (row.source.is_native() && row.has_parent_session)
        })
        .collect()
}


/// Split Web AllProjects rows into regular rows and temporary-workspace rows:
/// unnamed, tiny, native Pi sessions whose cwd sits lexically under the OS
/// temp root ([`std::env::temp_dir`]) — the historical test-harness shape.
///
/// The RPC marks the second bucket `temporary` so the sidebar can hide it by
/// default while keeping the rows searchable and keeping loaded/active
/// sessions visible: a recoverable view signal, never a backend filter or
/// deletion. The comparison is purely lexical — no file reads, no deletion —
/// and the TUI/repl catalog never calls this (Web AllProjects only).
#[must_use]
pub fn partition_web_noise_rows(
    rows: Vec<ResumeSelectorRow>,
) -> (Vec<ResumeSelectorRow>, Vec<ResumeSelectorRow>) {
    let temp_root = std::env::temp_dir();
    let mut regular = Vec::new();
    let mut temporary = Vec::new();
    for row in rows {
        if is_temporary_workspace_row(&row, &temp_root) {
            temporary.push(row);
        } else {
            regular.push(row);
        }
    }
    (regular, temporary)
}

fn is_temporary_workspace_row(row: &ResumeSelectorRow, temp_root: &Path) -> bool {
    row.source.is_native()
        && row.name.is_none()
        && row.size < WEB_NOISE_SESSION_MAX_BYTES
        && row.cwd.starts_with(temp_root)
}

/// Resolve a row or typed path/id to a native session path.
///
/// Native rows are always eligible. Foreign and `AlreadyImported` targets are
/// rechecked against `sources`, so a selector opened under an older policy
/// cannot bypass a settings reload. Typed input is matched only against the
/// same effective sources.
pub fn resolve_resume_selection(
    catalog: &SessionCatalog,
    request: &ResumeSelectionRequest,
    preferred_cwd: Option<&Path>,
    sources: &[SessionSourceKind],
) -> Result<ResumeSelectionResult, CatalogError> {
    let sources = normalize_resume_sources(sources);
    match request {
        ResumeSelectionRequest::Target(target) => match &target.status {
            CatalogRowStatus::Native => {
                if catalog
                    .native_session_kind(&target.source_path)
                    .is_workflow_internal()
                {
                    return Err(CatalogError::WorkflowInternalSession);
                }
                Ok(ResumeSelectionResult {
                    source: SessionSourceKind::NativePi,
                    path: target.source_path.clone(),
                    imported: false,
                    reused_existing: true,
                    native_no_copy: true,
                    message_count: None,
                    cancelled: false,
                    notice: None,
                })
            }
            CatalogRowStatus::AlreadyImported { native_path, .. } => {
                ensure_source_enabled(target.source, &sources, &target.source_path)?;
                if target.source.is_importable() {
                    // A listed foreign source can change before activation.
                    // Re-enter the shared import path for every foreign kind:
                    // unchanged lineage reuses the validated native copy,
                    // while changed content produces a fresh immutable import.
                    return imported_result(catalog.import_or_resume(
                        target.source,
                        &target.source_path,
                        preferred_cwd,
                    )?);
                }
                Ok(ResumeSelectionResult {
                    source: target.source,
                    path: native_path.clone(),
                    imported: false,
                    reused_existing: true,
                    native_no_copy: false,
                    message_count: None,
                    cancelled: false,
                    notice: None,
                })
            }
            CatalogRowStatus::Foreign => {
                ensure_source_enabled(target.source, &sources, &target.source_path)?;
                imported_result(catalog.import_or_resume(
                    target.source,
                    &target.source_path,
                    preferred_cwd,
                )?)
            }
        },
        ResumeSelectionRequest::Input(input) => {
            let (kind, path) = resolve_input_from_sources(catalog, input, &sources)?;
            if kind.is_native() {
                if catalog.native_session_kind(&path).is_workflow_internal() {
                    return Err(CatalogError::WorkflowInternalSession);
                }
                return Ok(ResumeSelectionResult {
                    source: kind,
                    path,
                    imported: false,
                    reused_existing: true,
                    native_no_copy: true,
                    message_count: None,
                    cancelled: false,
                    notice: None,
                });
            }
            imported_result(catalog.import_or_resume(kind, path, preferred_cwd)?)
        }
    }
}

/// Resolve/import and switch the shared application to the selected session.
pub async fn switch_resume_selection(
    application: &Application,
    catalog: &SessionCatalog,
    request: &ResumeSelectionRequest,
    preferred_cwd: Option<&Path>,
    sources: &[SessionSourceKind],
) -> Result<ResumeSelectionResult> {
    let result = resolve_resume_selection(catalog, request, preferred_cwd, sources)
        .context("resolving resume selection")?;
    let outcome = application
        .switch_session(&result.path)
        .await
        .with_context(|| format!("switching to session {}", result.path.display()))?;
    if !outcome.cancelled {
        // The resumed session owns its recorder id: rebind workflow storage
        // so its own workflows restore (and the previous session's do not
        // leak into the resumed view — T43 session scoping).
        crate::session_run::rebind_workflows_for_active_session(application)
            .await
            .context("rebinding workflow storage after session switch")?;
    }
    Ok(ResumeSelectionResult {
        cancelled: outcome.cancelled,
        notice: outcome.notice,
        ..result
    })
}

fn ensure_source_enabled(
    source: SessionSourceKind,
    sources: &[SessionSourceKind],
    path: &Path,
) -> Result<(), CatalogError> {
    if source.is_native() || sources.contains(&source) {
        return Ok(());
    }
    Err(CatalogError::SessionNotFound(path.display().to_string()))
}

fn resolve_input_from_sources(
    catalog: &SessionCatalog,
    input: &str,
    sources: &[SessionSourceKind],
) -> Result<(SessionSourceKind, PathBuf), CatalogError> {
    let input_path = Path::new(input);
    let candidate = expand_catalog_input(catalog, input_path);
    if candidate.is_file() {
        let classified = SessionSourceKind::ALL.into_iter().find(|kind| {
            if kind.is_native() && catalog.is_pi_catalog_path(&candidate) {
                return false;
            }
            catalog.is_safe_session_path(*kind, &candidate)
                || candidate.starts_with(catalog.root_for(*kind).path)
        });
        if let Some(kind) = classified {
            ensure_source_enabled(kind, sources, &candidate)?;
            return catalog
                .resolve_for(kind, &candidate)
                .map(|path| (kind, path));
        }
        if candidate.extension() == Some(OsStr::new("jsonl"))
            && !catalog.is_pi_catalog_path(&candidate)
        {
            return catalog
                .resolve_for(SessionSourceKind::NativePi, &candidate)
                .map(|path| (SessionSourceKind::NativePi, path));
        }
        return Err(CatalogError::SessionNotFound(
            candidate.display().to_string(),
        ));
    }

    let mut matches = Vec::new();
    for kind in sources {
        match catalog.resolve_for(*kind, input_path) {
            Ok(path) => matches.push((*kind, path)),
            Err(CatalogError::SessionNotFound(_)) => {}
            Err(CatalogError::AmbiguousSession {
                matches: ambiguous, ..
            }) => {
                matches.extend(ambiguous);
            }
            Err(error) => return Err(error),
        }
    }
    matches.sort_by(|left, right| left.1.cmp(&right.1));
    matches.dedup();
    match matches.as_slice() {
        [] => Err(CatalogError::SessionNotFound(input.to_owned())),
        [(kind, path)] => Ok((*kind, path.clone())),
        _ => Err(CatalogError::AmbiguousSession {
            input: input.to_owned(),
            matches,
        }),
    }
}

fn expand_catalog_input(catalog: &SessionCatalog, path: &Path) -> PathBuf {
    let expanded = if path == Path::new("~") {
        catalog.user_home().to_path_buf()
    } else if let Ok(rest) = path.strip_prefix("~/") {
        catalog.user_home().join(rest)
    } else if let Ok(rest) = path.strip_prefix("~") {
        catalog.user_home().join(rest)
    } else {
        path.to_path_buf()
    };
    if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir().map_or(expanded.clone(), |cwd| cwd.join(expanded))
    }
}

fn imported_result(resolved: ResolvedSession) -> Result<ResumeSelectionResult, CatalogError> {
    Ok(ResumeSelectionResult {
        source: resolved.kind,
        path: resolved.path,
        imported: !resolved.reused_existing,
        reused_existing: resolved.reused_existing,
        native_no_copy: resolved.native_no_copy,
        message_count: Some(resolved.messages.len()),
        cancelled: false,
        notice: None,
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn write_native(catalog: &SessionCatalog, id: &str, summary: &str) -> PathBuf {
        write_native_with_cwd(catalog, id, summary, Path::new("/tmp/work"))
    }

    fn write_native_with_cwd(
        catalog: &SessionCatalog,
        id: &str,
        summary: &str,
        cwd: &Path,
    ) -> PathBuf {
        let path = catalog
            .root_for(SessionSourceKind::NativePi)
            .path
            .join("--work--")
            .join(format!("{id}.jsonl"));
        fs::create_dir_all(path.parent().expect("native parent")).expect("native parent");
        let cwd = serde_json::to_string(&cwd.to_string_lossy()).expect("serialize cwd");
        fs::write(
            &path,
            format!(
                "{{\"type\":\"session\",\"version\":3,\"id\":\"{id}\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"cwd\":{cwd}}}\n{{\"type\":\"message\",\"id\":\"u\",\"parentId\":null,\"message\":{{\"role\":\"user\",\"content\":\"{summary}\"}}}}\n"
            ),
        )
        .expect("native fixture");
        path
    }

    fn write_codex(catalog: &SessionCatalog, id: &str, summary: &str) -> PathBuf {
        let path = catalog
            .root_for(SessionSourceKind::Codex)
            .path
            .join(format!("rollout-{id}.jsonl"));
        fs::create_dir_all(path.parent().expect("codex parent")).expect("codex parent");
        fs::write(
            &path,
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\",\"cwd\":\"/tmp/work\"}}}}\n{{\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"user\",\"content\":[{{\"type\":\"input_text\",\"text\":\"{summary}\"}}]}}}}\n{{\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{{\"type\":\"output_text\",\"text\":\"done\"}}]}}}}\n"
            ),
        )
        .expect("codex fixture");
        path
    }
    fn write_omp(catalog: &SessionCatalog, id: &str, summary: &str) -> PathBuf {
        let path = catalog
            .root_for(SessionSourceKind::Omp)
            .path
            .join("--work--")
            .join(format!("{id}.jsonl"));
        fs::create_dir_all(path.parent().expect("omp parent")).expect("omp parent");
        fs::write(
            &path,
            format!(
                "{{\"type\":\"session\",\"version\":3,\"id\":\"{id}\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"cwd\":\"/tmp/work\"}}\n{{\"type\":\"message\",\"id\":\"u\",\"parentId\":null,\"message\":{{\"role\":\"user\",\"content\":\"{summary}\"}}}}\n"
            ),
        )
        .expect("omp fixture");
        path
    }

    /// Write an OMP rotation chain (root → leaf) where every file after the
    /// first carries a `parentSession` header pointing at the previous file.
    fn write_omp_chain(catalog: &SessionCatalog, count: usize) -> Vec<PathBuf> {
        let directory = catalog
            .root_for(SessionSourceKind::Omp)
            .path
            .join("--chain--");
        fs::create_dir_all(&directory).expect("omp chain dir");
        let mut files = Vec::new();
        for index in 0..count {
            let id = format!("rc-chain-{index}");
            let path = directory.join(format!("{id}.jsonl"));
            let parent = files
                .last()
                .map(|previous: &PathBuf| format!(r#","parentSession":"{}""#, previous.display()))
                .unwrap_or_default();
            fs::write(
                &path,
                format!(
                    concat!(
                        r#"{{"type":"session","version":3,"id":"{id}","timestamp":"2026-01-01T00:00:00Z","cwd":"<workspace>/work"{parent}}}"#,
                        "\n",
                        r#"{{"type":"message","id":"u{index}","parentId":null,"timestamp":"2026-01-01T00:00:01Z","message":{{"role":"user","content":"{id} prompt"}}}}"#,
                        "\n",
                        r#"{{"type":"message","id":"a{index}","parentId":"u{index}","timestamp":"2026-01-01T00:00:02Z","message":{{"role":"assistant","content":"{id} reply"}}}}"#,
                        "\n"
                    ),
                    index = index,
                    id = id,
                    parent = parent
                ),
            )
            .expect("omp chain file");
            files.push(path);
        }
        files
    }

    #[test]
    fn web_default_sources_include_common_local_agents() {
        assert_eq!(
            web_resume_sources_from_settings(&pi_coding::Settings::default()),
            [
                SessionSourceKind::NativePi,
                SessionSourceKind::Pi,
                SessionSourceKind::Omp,
                SessionSourceKind::Codex,
                SessionSourceKind::Grok,
            ]
        );
    }

    #[test]
    fn web_explicit_empty_sources_remain_native_only() {
        let settings: pi_coding::Settings =
            serde_json::from_str(r#"{"sessionImportSources":[]}"#).expect("explicit empty sources");
        assert_eq!(
            web_resume_sources_from_settings(&settings),
            [SessionSourceKind::NativePi]
        );
    }

    #[test]
    fn web_explicit_sources_replace_the_fallback() {
        let settings: pi_coding::Settings =
            serde_json::from_str(r#"{"sessionImportSources":["codex"]}"#)
                .expect("explicit Codex source");
        assert_eq!(
            web_resume_sources_from_settings(&settings),
            [SessionSourceKind::NativePi, SessionSourceKind::Codex]
        );
    }

    #[test]
    fn web_explicit_pi_source_covers_immutable_pi_catalog() {
        let settings: pi_coding::Settings =
            serde_json::from_str(r#"{"sessionImportSources":["pi"]}"#).expect("explicit Pi source");
        assert_eq!(
            web_resume_sources_from_settings(&settings),
            [SessionSourceKind::NativePi, SessionSourceKind::Pi]
        );
    }

    fn write_pi_session(catalog: &SessionCatalog, id: &str) -> PathBuf {
        let path = catalog
            .root_for(SessionSourceKind::Pi)
            .path
            .join("project")
            .join(format!("{id}.jsonl"));
        fs::create_dir_all(path.parent().expect("Pi session parent"))
            .expect("create Pi session root");
        fs::write(
            &path,
            format!(
                concat!(
                    r#"{{"type":"session","version":3,"id":"{id}","timestamp":"2026-01-01T00:00:00Z","cwd":"<workspace>"}}"#,
                    "\n",
                    r#"{{"type":"message","id":"u","parentId":null,"timestamp":"2026-01-01T00:00:01Z","message":{{"role":"user","content":"pi overlap"}}}}"#,
                    "\n"
                ),
                id = id
            ),
        )
        .expect("write Pi session");
        path
    }

    fn assert_typed_pi_input_never_native_no_copy(catalog: &SessionCatalog, path: &Path) {
        assert!(catalog.native_and_pi_roots_overlap());
        let result = resolve_resume_selection(
            catalog,
            &ResumeSelectionRequest::Input(path.to_string_lossy().into_owned()),
            None,
            &[SessionSourceKind::NativePi, SessionSourceKind::Pi],
        );
        match result {
            Ok(selection) => {
                assert_eq!(selection.source, SessionSourceKind::Pi);
                assert!(!selection.native_no_copy);
                assert_ne!(selection.path, path);
            }
            Err(CatalogError::Import(_)) | Err(CatalogError::SessionNotFound(_)) => {}
            Err(error) => panic!("typed Pi path must not resume native/no-copy: {error:?}"),
        }
    }

    #[test]
    fn typed_pi_path_under_equal_root_overlap_is_not_native_no_copy() {
        let home = tempfile::tempdir().expect("home");
        let shared = home.path().join(".pi/agent/sessions");
        fs::create_dir_all(&shared).expect("shared catalog root");
        let catalog = SessionCatalog::new(home.path()).with_native_session_root(&shared);
        let path = write_pi_session(&catalog, "equal-root");
        assert_typed_pi_input_never_native_no_copy(&catalog, &path);
    }

    #[test]
    fn typed_pi_path_under_native_inside_pi_overlap_is_not_native_no_copy() {
        let home = tempfile::tempdir().expect("home");
        let pi_root = home.path().join(".pi/agent/sessions");
        let native_root = pi_root.join("rpi-native");
        fs::create_dir_all(&native_root).expect("nested native root");
        let catalog = SessionCatalog::new(home.path()).with_native_session_root(&native_root);
        let path = write_pi_session(&catalog, "native-inside-pi");
        assert_typed_pi_input_never_native_no_copy(&catalog, &path);
    }

    #[test]
    fn typed_pi_path_under_pi_inside_native_overlap_is_not_native_no_copy() {
        let home = tempfile::tempdir().expect("home");
        let native_root = home.path().join(".pi/agent");
        fs::create_dir_all(native_root.join("sessions")).expect("native parent of Pi");
        let catalog = SessionCatalog::new(home.path()).with_native_session_root(&native_root);
        let path = write_pi_session(&catalog, "pi-inside-native");
        assert_typed_pi_input_never_native_no_copy(&catalog, &path);
    }

    #[cfg(unix)]
    #[test]
    fn typed_pi_path_under_canonical_symlink_alias_is_not_native_no_copy() {
        use std::os::unix::fs::symlink;

        let home = tempfile::tempdir().expect("home");
        let pi_root = home.path().join(".pi/agent/sessions");
        fs::create_dir_all(&pi_root).expect("pi catalog root");
        let alias = home.path().join("native-alias");
        symlink(&pi_root, &alias).expect("alias native root onto Pi");
        let catalog = SessionCatalog::new(home.path()).with_native_session_root(&alias);
        let path = write_pi_session(&catalog, "symlink-alias");
        let alias_path = alias.join("project/symlink-alias.jsonl");
        assert_typed_pi_input_never_native_no_copy(&catalog, &path);
        assert_typed_pi_input_never_native_no_copy(&catalog, &alias_path);
    }

    #[test]
    fn web_typed_pi_path_imports_without_mutating_the_source() {
        let home = tempfile::tempdir().expect("home");
        let catalog = SessionCatalog::new(home.path());
        let source = catalog
            .root_for(SessionSourceKind::Pi)
            .path
            .join("project/pi-importable.jsonl");
        fs::create_dir_all(source.parent().expect("Pi session parent"))
            .expect("create Pi session root");
        fs::write(
            &source,
            concat!(
                r#"{"type":"session","version":3,"id":"pi-importable","timestamp":"2026-01-01T00:00:00Z","cwd":"<workspace>"}"#,
                "\n",
                r#"{"type":"message","id":"u","parentId":null,"timestamp":"2026-01-01T00:00:01Z","message":{"role":"user","content":"pi import prompt"}}"#,
                "\n"
            ),
        )
        .expect("write Pi session");
        let source_bytes = fs::read(&source).expect("Pi source bytes");
        let source_modified = fs::metadata(&source)
            .expect("Pi source metadata")
            .modified()
            .expect("mtime");

        let rows = load_resume_catalog(
            &catalog,
            &ResumeCatalogRequest {
                sources: vec![SessionSourceKind::NativePi, SessionSourceKind::Pi],
                ..ResumeCatalogRequest::default()
            },
        )
        .expect("Pi catalog listing")
        .rows;
        assert!(rows.iter().any(|row| row.source == SessionSourceKind::Pi));

        let selection = resolve_resume_selection(
            &catalog,
            &ResumeSelectionRequest::Input(source.to_string_lossy().into_owned()),
            None,
            &[SessionSourceKind::NativePi, SessionSourceKind::Pi],
        )
        .expect("Pi path imports");
        assert_eq!(selection.source, SessionSourceKind::Pi);
        assert!(selection.imported);
        assert!(!selection.native_no_copy);
        assert_ne!(selection.path, source);
        assert_eq!(selection.message_count, Some(1));
        assert_eq!(fs::read(&source).expect("Pi source after"), source_bytes);
        assert_eq!(
            fs::metadata(&source)
                .expect("Pi source metadata after")
                .modified()
                .expect("mtime after"),
            source_modified,
        );
        assert!(catalog.is_safe_session_path(SessionSourceKind::NativePi, &selection.path));
    }

    #[test]
    fn pi_already_imported_target_revalidates_lineage_before_reuse() {
        let home = tempfile::tempdir().expect("home");
        let catalog = SessionCatalog::new(home.path());
        let source = write_pi_session(&catalog, "pi-stale-row");
        let sources = [SessionSourceKind::NativePi, SessionSourceKind::Pi];
        let first = resolve_resume_selection(
            &catalog,
            &ResumeSelectionRequest::Input(source.to_string_lossy().into_owned()),
            None,
            &sources,
        )
        .expect("first Pi import");
        assert!(first.imported);

        fs::write(
            &source,
            format!(
                concat!(
                    r#"{{"type":"session","version":3,"id":"pi-stale-row","timestamp":"2026-01-01T00:00:00Z","cwd":"<workspace>"}}"#,
                    "\n",
                    r#"{{"type":"message","id":"u","parentId":null,"timestamp":"2026-01-01T00:00:01Z","message":{{"role":"user","content":"pi changed after listing"}}}}"#,
                    "\n"
                )
            ),
        )
        .expect("change Pi source");

        let changed = resolve_resume_selection(
            &catalog,
            &ResumeSelectionRequest::Target(ResumeSelectorTarget {
                source: SessionSourceKind::Pi,
                source_path: source.clone(),
                status: CatalogRowStatus::AlreadyImported {
                    native_id: "stale".to_owned(),
                    native_path: first.path.clone(),
                },
            }),
            None,
            &sources,
        )
        .expect("changed Pi source reimports");
        assert!(changed.imported);
        assert_ne!(changed.path, first.path);
        assert!(
            fs::read_to_string(&changed.path)
                .expect("changed import body")
                .contains("pi changed after listing")
        );

        let reused = resolve_resume_selection(
            &catalog,
            &ResumeSelectionRequest::Target(ResumeSelectorTarget {
                source: SessionSourceKind::Pi,
                source_path: source,
                status: CatalogRowStatus::AlreadyImported {
                    native_id: "fresh".to_owned(),
                    native_path: changed.path.clone(),
                },
            }),
            None,
            &sources,
        )
        .expect("unchanged Pi source reuses import");
        assert!(!reused.imported);
        assert!(reused.reused_existing);
        assert_eq!(reused.path, changed.path);
    }

    /// Product decision: imported foreign rows STAY VISIBLE next to their
    /// native copy. After an import the listing carries BOTH rows: the
    /// native copy (Native) and the foreign source (AlreadyImported with the
    /// native copy's path) so the sidebar can badge the import and resolve
    /// either representation.
    #[test]
    fn imported_foreign_row_remains_listed_with_already_imported_status() {
        let home = tempfile::tempdir().expect("home");
        let catalog = SessionCatalog::new(home.path());
        let source = write_codex(&catalog, "web-coalesce", "foreign summary");
        resolve_resume_selection(
            &catalog,
            &ResumeSelectionRequest::Input(source.to_string_lossy().into_owned()),
            Some(Path::new("/tmp/work")),
            &[SessionSourceKind::NativePi, SessionSourceKind::Codex],
        )
        .expect("import foreign row");
        let rows = load_resume_catalog(
            &catalog,
            &ResumeCatalogRequest {
                sources: vec![SessionSourceKind::NativePi, SessionSourceKind::Codex],
                ..ResumeCatalogRequest::default()
            },
        )
        .expect("catalog")
        .rows;

        let native_row = rows
            .iter()
            .find(|row| row.source == SessionSourceKind::NativePi)
            .expect("the native imported copy stays listed");
        assert!(matches!(native_row.status, CatalogRowStatus::Native));
        let foreign_row = rows
            .iter()
            .find(|row| row.source == SessionSourceKind::Codex)
            .expect("the imported foreign row remains listed");
        let CatalogRowStatus::AlreadyImported { native_path, .. } = &foreign_row.status else {
            panic!("the foreign row must carry AlreadyImported status: {:?}", foreign_row.status);
        };
        assert_eq!(
            native_path, &native_row.path,
            "AlreadyImported points at the native copy so the frontend can resolve it"
        );
    }
    #[test]
    fn web_noise_filter_uses_strict_ten_kib_and_zero_messages() {
        fn row(size: u64, message_count: Option<usize>) -> ResumeSelectorRow {
            ResumeSelectorRow {
                source: SessionSourceKind::NativePi,
                source_badge: "pi",
                session_id: format!("row-{size}-{}", message_count.unwrap_or(99)),
                cwd: PathBuf::from("<workspace>"),
                modified_epoch: 0.0,
                display_time: String::new(),
                summary: String::new(),
                path: PathBuf::from(format!("row-{size}.jsonl")),
                size,
                message_count,
                name: None,
                status: CatalogRowStatus::Native,
                search_text: String::new(),
                message_blob: String::new(),
                has_parent_session: false,
                session_kind: SessionKind::User,
                agent_session: false,
                bootstrap_only: false,
            }
        }
        let rows = vec![
            row(10_239, Some(0)),
            row(10_240, Some(0)),
            row(1, Some(1)),
            row(1, None),
        ];

        let visible = filter_web_noise_rows(rows);
        assert_eq!(visible.len(), 3);
        assert!(
            visible
                .iter()
                .any(|row| row.size == 10_240 && row.message_count == Some(0))
        );
        assert!(visible.iter().any(|row| row.message_count == Some(1)));
        assert!(visible.iter().any(|row| row.message_count.is_none()));
    }

    #[test]
    fn web_noise_filter_keeps_only_parent_linked_zero_message_native_rows() {
        let home = tempfile::tempdir().expect("home");
        let native = tempfile::tempdir().expect("native root");
        let catalog = SessionCatalog::new(home.path()).with_native_session_root(native.path());
        let directory = native.path();
        fs::create_dir_all(directory).expect("native parent");
        let cwd = real_workspace_cwd();
        let cwd_json = serde_json::to_string(&cwd).expect("serialize cwd");
        fs::write(
            directory.join("noise.jsonl"),
            format!(
                "{{\"type\":\"session\",\"version\":3,\"id\":\"noise\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"cwd\":{cwd_json}}}\n"
            ),
        )
        .expect("noise fixture");
        fs::write(
            directory.join("fork.jsonl"),
            format!(
                "{{\"type\":\"session\",\"version\":3,\"id\":\"fork\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"cwd\":{cwd_json},\"parentSession\":\"parent.jsonl\"}}\n"
            ),
        )
        .expect("fork fixture");
        let rows = load_resume_catalog(&catalog, &ResumeCatalogRequest::default())
            .expect("catalog")
            .rows;
        assert!(
            rows.iter()
                .find(|row| row.session_id == "fork")
                .is_some_and(|row| row.has_parent_session)
        );
        let visible = filter_web_noise_rows(rows);
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].session_id, "fork");
    }

    fn noise_row(
        source: SessionSourceKind,
        cwd: &Path,
        size: u64,
        message_count: Option<usize>,
        name: Option<&str>,
    ) -> ResumeSelectorRow {
        ResumeSelectorRow {
            source,
            source_badge: source.label(),
            session_id: format!("noise-row-{}-{}", source.as_str(), size),
            cwd: cwd.to_path_buf(),
            modified_epoch: 0.0,
            display_time: String::new(),
            summary: String::new(),
            path: PathBuf::from("noise-row.jsonl"),
            size,
            message_count,
            name: name.map(str::to_owned),
            status: CatalogRowStatus::Native,
            has_parent_session: false,
            session_kind: SessionKind::User,
            agent_session: false,
            bootstrap_only: false,
            search_text: String::new(),
            message_blob: String::new(),
        }
    }

    /// Lexically under the OS temp root — the historical test-workspace shape.
    fn temp_workspace_cwd() -> PathBuf {
        std::env::temp_dir().join("pi-catalog-noise-workspace")
    }

    /// A real-workspace path that can never lexically start with the OS temp
    /// root, so "outside temp" fixtures stay hermetic on any platform.
    fn real_workspace_cwd() -> PathBuf {
        let cwd = PathBuf::from("<workspace>/projects/workspace");
        assert!(
            !cwd.starts_with(std::env::temp_dir()),
            "real-workspace fixture must not live under the OS temp root"
        );
        cwd
    }

    #[test]
    fn web_noise_partition_flags_unnamed_small_native_temp_rows() {
        let temp = temp_workspace_cwd();
        let rows = vec![
            // Tiny harness fixture with real messages: never backend-dropped,
            // but partitioned as temporary.
            noise_row(SessionSourceKind::NativePi, &temp, 512, Some(2), None),
            // Just under the 10 KiB threshold, still unnamed and in temp.
            noise_row(
                SessionSourceKind::NativePi,
                &temp,
                WEB_NOISE_SESSION_MAX_BYTES - 1,
                Some(1),
                None,
            ),
        ];
        // The backend filter keeps both (they carry real messages); the
        // recoverable-view partition flags both as temporary.
        assert_eq!(filter_web_noise_rows(rows.clone()).len(), 2);
        let (regular, temporary) = partition_web_noise_rows(rows);
        assert!(regular.is_empty(), "no regular rows: {regular:?}");
        assert_eq!(temporary.len(), 2);
        assert!(temporary.iter().all(|row| row.message_count.is_some()));
    }

    #[test]
    fn web_noise_partition_keeps_short_user_rows_outside_temp_regular() {
        let real = real_workspace_cwd();
        let rows = vec![
            noise_row(SessionSourceKind::NativePi, &real, 512, Some(2), None),
            noise_row(SessionSourceKind::NativePi, &real, 1, Some(1), None),
        ];
        assert_eq!(filter_web_noise_rows(rows.clone()).len(), 2);
        let (regular, temporary) = partition_web_noise_rows(rows);
        assert_eq!(regular.len(), 2);
        assert!(
            temporary.is_empty(),
            "non-temp short rows are never temporary"
        );
    }

    #[test]
    fn web_noise_partition_keeps_named_temp_rows_regular() {
        let temp = temp_workspace_cwd();
        let rows = vec![noise_row(
            SessionSourceKind::NativePi,
            &temp,
            512,
            Some(1),
            Some("my experiment"),
        )];
        assert_eq!(filter_web_noise_rows(rows.clone()).len(), 1);
        let (regular, temporary) = partition_web_noise_rows(rows);
        assert_eq!(regular.len(), 1, "named rows are never temporary");
        assert!(temporary.is_empty());
    }

    #[test]
    fn web_noise_partition_keeps_foreign_temp_rows_regular() {
        let temp = temp_workspace_cwd();
        let rows = vec![
            noise_row(SessionSourceKind::Codex, &temp, 512, Some(1), None),
            noise_row(SessionSourceKind::Omp, &temp, 512, Some(1), None),
        ];
        assert_eq!(filter_web_noise_rows(rows.clone()).len(), 2);
        let (regular, temporary) = partition_web_noise_rows(rows);
        assert_eq!(regular.len(), 2, "foreign rows are never temporary");
        assert!(temporary.is_empty());
    }

    #[test]
    fn web_noise_partition_temp_boundary_at_ten_kib() {
        let temp = temp_workspace_cwd();
        let rows = vec![
            noise_row(
                SessionSourceKind::NativePi,
                &temp,
                WEB_NOISE_SESSION_MAX_BYTES - 1,
                Some(1),
                None,
            ),
            noise_row(
                SessionSourceKind::NativePi,
                &temp,
                WEB_NOISE_SESSION_MAX_BYTES,
                Some(1),
                None,
            ),
            // >=10 KiB survives even with zero messages (unchanged boundary).
            noise_row(
                SessionSourceKind::NativePi,
                &temp,
                WEB_NOISE_SESSION_MAX_BYTES,
                Some(0),
                None,
            ),
        ];
        // Zero-message filter keeps all three (the 10 KiB boundary is
        // unchanged); only the sub-10 KiB unnamed temp row is temporary.
        let visible = filter_web_noise_rows(rows);
        assert_eq!(visible.len(), 3);
        let (regular, temporary) = partition_web_noise_rows(visible);
        assert_eq!(temporary.len(), 1);
        assert_eq!(temporary[0].size, WEB_NOISE_SESSION_MAX_BYTES - 1);
        assert_eq!(regular.len(), 2);
        assert!(
            regular
                .iter()
                .all(|row| row.size >= WEB_NOISE_SESSION_MAX_BYTES)
        );
    }

    #[test]
    fn web_noise_partition_flags_temp_fixture_through_real_catalog() {
        let home = tempfile::tempdir().expect("home");
        let catalog = SessionCatalog::new(home.path());
        write_native_with_cwd(
            &catalog,
            "tmp-fixture",
            "harness probe",
            &temp_workspace_cwd(),
        );
        write_native_with_cwd(&catalog, "user-short", "hello there", &real_workspace_cwd());
        let rows = load_resume_catalog(&catalog, &ResumeCatalogRequest::default())
            .expect("native catalog")
            .rows;
        assert_eq!(rows.len(), 2, "catalog must see both seeds: {rows:?}");

        // Both seeds carry messages, so the backend filter keeps both; the
        // partition flags exactly the temp-workspace fixture.
        let visible = filter_web_noise_rows(rows);
        assert_eq!(visible.len(), 2, "filter keeps both message-carrying rows");
        let (regular, temporary) = partition_web_noise_rows(visible);
        assert_eq!(temporary.len(), 1);
        assert_eq!(temporary[0].session_id, "tmp-fixture");
        assert_eq!(regular.len(), 1);
        assert_eq!(regular[0].session_id, "user-short");
    }

    #[test]
    fn web_noise_filter_keeps_small_grok_session_with_chat_messages() {
        let home = tempfile::tempdir().expect("home");
        let catalog = SessionCatalog::new(home.path());
        let directory = catalog
            .root_for(SessionSourceKind::Grok)
            .path
            .join("work")
            .join("grok-small");
        fs::create_dir_all(&directory).expect("grok directory");
        fs::write(
            directory.join("summary.json"),
            r#"{"info":{"id":"grok-small","cwd":"<workspace>"}}"#,
        )
        .expect("grok summary");
        fs::write(
            directory.join("chat_history.jsonl"),
            "{\"role\":\"user\",\"content\":\"keep me\"}\n",
        )
        .expect("grok chat");
        let rows = load_resume_catalog(
            &catalog,
            &ResumeCatalogRequest {
                sources: vec![SessionSourceKind::Grok],
                ..ResumeCatalogRequest::default()
            },
        )
        .expect("grok catalog")
        .rows;
        assert!(rows[0].size < WEB_NOISE_SESSION_MAX_BYTES);
        assert_eq!(rows[0].message_count, Some(1));
        assert_eq!(filter_web_noise_rows(rows).len(), 1);
    }

    #[test]
    fn web_noise_filter_keeps_native_image_only_outside_temp() {
        let home = tempfile::tempdir().expect("home");
        let catalog = SessionCatalog::new(home.path());
        let cwd = Path::new("<workspace>/projects/image-work");
        assert!(
            !cwd.starts_with(std::env::temp_dir()),
            "image-only fixture cwd must stay outside the OS temp root"
        );
        let path = catalog
            .root_for(SessionSourceKind::NativePi)
            .path
            .join("--image--")
            .join("native-image.jsonl");
        fs::create_dir_all(path.parent().expect("parent")).expect("parent");
        let cwd = serde_json::to_string(&cwd.to_string_lossy()).expect("serialize cwd");
        fs::write(
            &path,
            format!(
                concat!(
                    r#"{{"type":"session","version":3,"id":"native-image","timestamp":"2026-01-01T00:00:00Z","cwd":{}}}"#,
                    "\n",
                    r#"{{"type":"message","id":"u","parentId":null,"timestamp":"2026-01-01T00:00:01Z","message":{{"role":"user","content":[{{"type":"image","data":"aW1n","mimeType":"image/png"}}]}}}}"#,
                    "\n"
                ),
                cwd,
            ),
        )
        .expect("native image-only fixture");
        let rows = load_resume_catalog(
            &catalog,
            &ResumeCatalogRequest {
                sources: vec![SessionSourceKind::NativePi],
                ..ResumeCatalogRequest::default()
            },
        )
        .expect("native catalog")
        .rows;
        assert_eq!(rows.len(), 1);
        assert!(rows[0].size < WEB_NOISE_SESSION_MAX_BYTES);
        assert_eq!(rows[0].message_count, Some(1));
        // Image-only turn is meaningful, and the cwd is a real workspace (not
        // the OS temp root), so the noise filter keeps it despite being under
        // 10 KiB.
        assert_eq!(filter_web_noise_rows(rows).len(), 1);
    }

    #[test]
    fn web_noise_filter_keeps_omp_image_only_under_ten_kib() {
        let home = tempfile::tempdir().expect("home");
        let catalog = SessionCatalog::new(home.path());
        let path = catalog
            .root_for(SessionSourceKind::Omp)
            .path
            .join("--image--")
            .join("omp-image.jsonl");
        fs::create_dir_all(path.parent().expect("parent")).expect("parent");
        fs::write(
            &path,
            concat!(
                r#"{"type":"session","version":3,"id":"omp-image","timestamp":"2026-01-01T00:00:00Z","cwd":"/tmp/image"}"#,
                "\n",
                r#"{"type":"message","id":"u","parentId":null,"timestamp":"2026-01-01T00:00:01Z","message":{"role":"user","content":[{"type":"image","data":"aW1n","mimeType":"image/png"}]}}"#,
                "\n"
            ),
        )
        .expect("omp image-only fixture");
        let rows = load_resume_catalog(
            &catalog,
            &ResumeCatalogRequest {
                sources: vec![SessionSourceKind::NativePi, SessionSourceKind::Omp],
                include_foreign: true,
                ..ResumeCatalogRequest::default()
            },
        )
        .expect("omp catalog")
        .rows;
        let omp = rows
            .iter()
            .find(|row| row.source == SessionSourceKind::Omp)
            .expect("omp row");
        assert!(omp.size < WEB_NOISE_SESSION_MAX_BYTES);
        assert_eq!(omp.message_count, Some(1));
        assert_eq!(filter_web_noise_rows(rows).len(), 1);
    }

    #[test]
    fn web_noise_filter_keeps_grok_large_non_convertible_chat_by_aggregate_size() {
        let home = tempfile::tempdir().expect("home");
        let catalog = SessionCatalog::new(home.path());
        let directory = catalog
            .root_for(SessionSourceKind::Grok)
            .path
            .join("agg-cwd")
            .join("grok-agg");
        fs::create_dir_all(&directory).expect("grok directory");
        fs::write(
            directory.join("summary.json"),
            r#"{"info":{"id":"grok-agg","cwd":"<workspace>"}}"#,
        )
        .expect("grok summary");
        // Non-convertible system records padded past 10 KiB: message_count
        // stays zero, so the row survives only via aggregate size.
        let padding = "x".repeat(11_000);
        fs::write(
            directory.join("chat_history.jsonl"),
            format!(r#"{{"type":"system","content":"{padding}"}}"#),
        )
        .expect("grok chat");
        let rows = load_resume_catalog(
            &catalog,
            &ResumeCatalogRequest {
                sources: vec![SessionSourceKind::Grok],
                include_foreign: true,
                ..ResumeCatalogRequest::default()
            },
        )
        .expect("grok catalog")
        .rows;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].message_count, Some(0));
        assert!(rows[0].size >= WEB_NOISE_SESSION_MAX_BYTES);
        assert_eq!(filter_web_noise_rows(rows).len(), 1);
    }

    #[test]
    fn default_catalog_request_is_native_only() {
        let home = tempfile::tempdir().expect("home");
        let catalog = SessionCatalog::new(home.path());
        write_native(&catalog, "native-default", "native visible");
        write_omp(&catalog, "omp-hidden", "omp hidden");
        write_codex(&catalog, "codex-hidden", "codex hidden");

        let rows = load_resume_catalog(&catalog, &ResumeCatalogRequest::default())
            .expect("native-only catalog")
            .rows;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].source, SessionSourceKind::NativePi);
    }

    #[test]
    fn omp_allowlist_enables_catalog_and_typed_resolution() {
        let home = tempfile::tempdir().expect("home");
        let catalog = SessionCatalog::new(home.path());
        let source = write_omp(&catalog, "omp-enabled", "omp visible");
        write_codex(&catalog, "codex-disabled", "codex hidden");
        let sources = [SessionSourceKind::NativePi, SessionSourceKind::Omp];

        let rows = load_resume_catalog(
            &catalog,
            &ResumeCatalogRequest {
                sources: sources.to_vec(),
                ..ResumeCatalogRequest::default()
            },
        )
        .expect("omp catalog")
        .rows;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].source, SessionSourceKind::Omp);

        let resolved = resolve_resume_selection(
            &catalog,
            &ResumeSelectionRequest::Input("omp-enabled".to_owned()),
            Some(Path::new("/tmp/work")),
            &sources,
        )
        .expect("enabled omp resolution");
        assert_eq!(resolved.source, SessionSourceKind::Omp);
        assert_ne!(resolved.path, source);
    }

    #[test]
    fn disabled_foreign_path_id_and_stale_targets_are_rejected() {
        let home = tempfile::tempdir().expect("home");
        let catalog = SessionCatalog::new(home.path());
        let source = write_codex(&catalog, "codex-disabled", "disabled");
        let native = write_native(&catalog, "native-allowed", "native");
        let native_only = [SessionSourceKind::NativePi];

        for request in [
            ResumeSelectionRequest::Input(source.to_string_lossy().into_owned()),
            ResumeSelectionRequest::Input("codex-disabled".to_owned()),
            ResumeSelectionRequest::Target(ResumeSelectorTarget {
                source: SessionSourceKind::Codex,
                source_path: source.clone(),
                status: CatalogRowStatus::Foreign,
            }),
            ResumeSelectionRequest::Target(ResumeSelectorTarget {
                source: SessionSourceKind::Codex,
                source_path: source.clone(),
                status: CatalogRowStatus::AlreadyImported {
                    native_id: "native-allowed".to_owned(),
                    native_path: native.clone(),
                },
            }),
        ] {
            assert!(matches!(
                resolve_resume_selection(&catalog, &request, None, &native_only),
                Err(CatalogError::SessionNotFound(_))
            ));
        }

        let native_target = resolve_resume_selection(
            &catalog,
            &ResumeSelectionRequest::Target(ResumeSelectorTarget {
                source: SessionSourceKind::NativePi,
                source_path: native.clone(),
                status: CatalogRowStatus::Native,
            }),
            None,
            &[],
        )
        .expect("native target remains allowed");
        assert_eq!(native_target.path, native);
    }

    #[test]
    fn selector_model_exposes_badges_search_sort_and_status() {
        let home = tempfile::tempdir().expect("home");
        let catalog = SessionCatalog::new(home.path());
        write_native(&catalog, "pi-row", "native summary");
        write_codex(&catalog, "codex-row", "foreign needle");

        let result = load_resume_catalog(
            &catalog,
            &ResumeCatalogRequest {
                query: Some("foreign codex".to_owned()),
                sources: vec![SessionSourceKind::NativePi, SessionSourceKind::Codex],
                sort: CatalogSort::Name,
                ..ResumeCatalogRequest::default()
            },
        )
        .expect("catalog result");
        assert_eq!(result.rows.len(), 1);
        let row = &result.rows[0];
        assert_eq!(row.source_badge, "codex");
        assert_eq!(row.cwd, PathBuf::from("/tmp/work"));
        assert_eq!(row.summary, "foreign needle");
        assert!(row.modified_epoch > 0.0);
        assert_eq!(row.status, CatalogRowStatus::Foreign);
        assert!(row.search_text.contains("foreign needle"));
        assert!(row.search_text.contains("codex-row"));
    }

    #[test]
    fn foreign_selection_is_idempotent_and_already_imported_switches_directly() {
        let home = tempfile::tempdir().expect("home");
        let catalog = SessionCatalog::new(home.path());
        let source = write_codex(&catalog, "same-id", "import once");
        let target = ResumeSelectorTarget {
            source: SessionSourceKind::Codex,
            source_path: source,
            status: CatalogRowStatus::Foreign,
        };
        let first = resolve_resume_selection(
            &catalog,
            &ResumeSelectionRequest::Target(target),
            Some(Path::new("/tmp/work")),
            &[SessionSourceKind::NativePi, SessionSourceKind::Codex],
        )
        .expect("first import");
        assert!(first.imported);

        let rows = load_resume_catalog(
            &catalog,
            &ResumeCatalogRequest {
                sources: vec![SessionSourceKind::NativePi, SessionSourceKind::Codex],
                ..ResumeCatalogRequest::default()
            },
        )
        .expect("reloaded catalog")
        .rows;
        let row = rows
            .iter()
            .find(|row| row.source == SessionSourceKind::Codex)
            .expect("codex row");
        assert!(matches!(
            row.status,
            CatalogRowStatus::AlreadyImported { .. }
        ));
        let second = resolve_resume_selection(
            &catalog,
            &ResumeSelectionRequest::Target(ResumeSelectorTarget::from(row)),
            Some(Path::new("/tmp/work")),
            &[SessionSourceKind::NativePi, SessionSourceKind::Codex],
        )
        .expect("direct existing selection");
        assert_eq!(second.path, first.path);
        assert!(!second.imported);
        assert!(second.reused_existing);
    }

    /// An OMP `AlreadyImported` row must NEVER short-circuit to the stored
    /// native path: the identity-only status can be stale between list and
    /// click (an ancestor mutated after the last scan), so selection re-enters
    /// `import_or_resume`, which reuses when the current chain fingerprint
    /// matches and emits a fresh full-history import otherwise.
    #[test]
    fn omp_already_imported_target_reenters_import_or_resume() {
        let home = tempfile::tempdir().expect("home");
        let catalog = SessionCatalog::new(home.path());
        let chain = write_omp_chain(&catalog, 3);
        let leaf = chain[2].clone();
        let sources = [SessionSourceKind::NativePi, SessionSourceKind::Omp];

        let first = resolve_resume_selection(
            &catalog,
            &ResumeSelectionRequest::Input(leaf.to_string_lossy().into_owned()),
            None,
            &sources,
        )
        .expect("first chain import");
        assert!(first.imported);
        let stale_path = first.path.clone();

        // Mutate the middle ancestor (content + mtime); the stored native copy
        // is now stale while the leaf row identity is unchanged.
        fs::write(
            &chain[1],
            fs::read_to_string(&chain[1])
                .expect("mid body")
                .replace("rc-chain-1 prompt", "rc-chain-1 prompt MUTATED"),
        )
        .expect("mutate mid");

        // A stale AlreadyImported target captured before the mutation must not
        // short-circuit to the old native copy.
        let fresh = resolve_resume_selection(
            &catalog,
            &ResumeSelectionRequest::Target(ResumeSelectorTarget {
                source: SessionSourceKind::Omp,
                source_path: leaf.clone(),
                status: CatalogRowStatus::AlreadyImported {
                    native_id: "stale".to_owned(),
                    native_path: stale_path.clone(),
                },
            }),
            None,
            &sources,
        )
        .expect("stale target must re-enter import");
        assert_ne!(
            fresh.path, stale_path,
            "stale native copy must not be selected"
        );
        assert!(
            fresh.imported,
            "ancestor mutation must produce a fresh import"
        );
        assert!(fresh.path.is_file());
        let fresh_body = fs::read_to_string(&fresh.path).expect("fresh body");
        assert!(
            fresh_body.contains("rc-chain-1 prompt MUTATED"),
            "updated ancestor transcript required in the fresh import"
        );
        assert!(
            fresh_body.contains("rc-chain-0 prompt"),
            "full chain retained"
        );

        // A fresh scan row (exact fingerprint match) resolves to the same
        // fresh import without a duplicate.
        let rows = load_resume_catalog(
            &catalog,
            &ResumeCatalogRequest {
                sources: sources.to_vec(),
                ..ResumeCatalogRequest::default()
            },
        )
        .expect("reloaded catalog")
        .rows;
        let row = rows
            .iter()
            .find(|row| row.session_id == "rc-chain-2")
            .expect("omp leaf row");
        let direct = resolve_resume_selection(
            &catalog,
            &ResumeSelectionRequest::Target(ResumeSelectorTarget::from(row)),
            None,
            &sources,
        )
        .expect("fresh row selection");
        assert_eq!(
            direct.path, fresh.path,
            "fresh row resolves the matching import"
        );
        assert!(direct.reused_existing);
    }

    #[test]
    fn foreign_import_uses_exact_native_session_root() {
        let home = tempfile::tempdir().expect("home");
        let native = tempfile::tempdir().expect("native root");
        let catalog = SessionCatalog::new(home.path()).with_native_session_root(native.path());
        let source = write_codex(&catalog, "custom-root", "import here");
        let result = resolve_resume_selection(
            &catalog,
            &ResumeSelectionRequest::Input(source.to_string_lossy().into_owned()),
            Some(Path::new("/tmp/work")),
            &[SessionSourceKind::NativePi, SessionSourceKind::Codex],
        )
        .expect("foreign import");
        assert_eq!(result.path.parent(), Some(native.path()));
        assert!(result.path.is_file());
    }

    #[test]
    fn native_input_is_no_copy() {
        let home = tempfile::tempdir().expect("home");
        let catalog = SessionCatalog::new(home.path());
        let path = write_native(&catalog, "native-prefix", "keep bytes");
        let before = fs::read(&path).expect("before");
        let result = resolve_resume_selection(
            &catalog,
            &ResumeSelectionRequest::Input("native-pre".to_owned()),
            None,
            &[SessionSourceKind::NativePi],
        )
        .expect("native prefix");
        assert!(result.native_no_copy);
        assert_eq!(result.path, path);
        assert_eq!(fs::read(&path).expect("after"), before);
    }
    #[test]
    fn typed_resume_resolves_exact_id_and_rejects_ambiguous_prefix() {
        let home = tempfile::tempdir().expect("home");
        let catalog = SessionCatalog::new(home.path());
        let exact_path = write_native(&catalog, "shared-alpha", "first");
        write_native(&catalog, "shared-beta", "second");

        let exact = resolve_resume_selection(
            &catalog,
            &ResumeSelectionRequest::Input("shared-alpha".to_owned()),
            None,
            &[SessionSourceKind::NativePi],
        )
        .expect("exact id");
        assert_eq!(exact.path, exact_path);

        let error = resolve_resume_selection(
            &catalog,
            &ResumeSelectionRequest::Input("shared-".to_owned()),
            None,
            &[SessionSourceKind::NativePi],
        )
        .expect_err("ambiguous prefix");
        assert!(matches!(error, CatalogError::AmbiguousSession { .. }));
    }

    /// The selector row layer passes the external-session classification
    /// through untouched: a Codex/Slock agent workspace row (`.slock/agents/`)
    /// with only bootstrap content carries both flags; a genuine user session
    /// in a real project carries neither. Flags are additive view signals —
    /// both rows stay listed, nothing is filtered.
    #[test]
    fn resume_rows_carry_external_session_classification() {
        let home = tempfile::tempdir().expect("home");
        let codex_home = home.path().join("codex-home");
        let catalog = SessionCatalog::new(home.path()).with_codex_home(&codex_home);
        let agent_cwd = home.path().join(".slock/agents/agent-7");
        fs::create_dir_all(&agent_cwd).expect("agent workspace");

        let bootstrap_text =
            "<environment_context>\nagent mode\n</environment_context><user_instructions>follow AGENTS.md</user_instructions>";
        let agent_path = catalog.root_for(SessionSourceKind::Codex).path;
        fs::create_dir_all(&agent_path).expect("codex root");
        fs::write(
            agent_path.join("rollout-agent.jsonl"),
            format!(
                concat!(
                    r#"{{"timestamp":"2026-02-01T00:00:00Z","type":"session_meta","payload":{{"id":"agent-row","cwd":{} }}}}"#,
                    "\n",
                    r#"{{"timestamp":"2026-02-01T00:00:01Z","type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":{}}}]}}}}"#,
                    "\n"
                ),
                serde_json::to_string(&agent_cwd.to_string_lossy()).expect("cwd"),
                serde_json::to_string(bootstrap_text).expect("text"),
            ),
        )
        .expect("agent fixture");
        let user_path = write_codex(&catalog, "user-session", "fix the login bug");
        let _ = user_path;

        let rows = load_resume_catalog(
            &catalog,
            &ResumeCatalogRequest {
                sources: vec![SessionSourceKind::Codex],
                ..ResumeCatalogRequest::default()
            },
        )
        .expect("catalog")
        .rows;
        assert_eq!(rows.len(), 2, "no backend filtering: {rows:?}");

        let agent = rows
            .iter()
            .find(|row| row.session_id == "agent-row")
            .expect("agent row");
        assert!(agent.agent_session, "agent workspace row: {:?}", agent.cwd);
        assert!(agent.bootstrap_only, "bootstrap-only content row");

        let user = rows
            .iter()
            .find(|row| row.session_id == "user-session")
            .expect("user row");
        assert!(!user.agent_session, "real project cwd stays real");
        assert!(!user.bootstrap_only, "genuine prompt is not bootstrap-only");
    }

    fn write_native_with_kind(
        catalog: &SessionCatalog,
        id: &str,
        session_kind: SessionKind,
    ) -> PathBuf {
        let path = catalog
            .root_for(SessionSourceKind::NativePi)
            .path
            .join("--work--")
            .join(format!("{id}.jsonl"));
        fs::create_dir_all(path.parent().expect("native parent")).expect("native parent");
        fs::write(
            &path,
            format!(
                concat!(
                    r#"{{"type":"session","version":3,"id":"{}","timestamp":"2026-01-01T00:00:00Z","cwd":"<workspace>/work","sessionKind":"{}"}}"#,
                    "\n",
                    r#"{{"type":"message","id":"u","parentId":null,"message":{{"role":"user","content":"workflow turn"}}}}"#,
                    "\n"
                ),
                id,
                session_kind.header_value()
            ),
        )
        .expect("native fixture");
        path
    }

    #[test]
    fn load_resume_catalog_hides_workflow_internal_rows_and_reports_user_kind() {
        let home = tempfile::tempdir().expect("home");
        let catalog = SessionCatalog::new(home.path());
        write_native(&catalog, "user-row", "genuine user prompt");
        write_native_with_kind(&catalog, "supervisor-row", SessionKind::WorkflowSupervisor);

        let rows = load_resume_catalog(
            &catalog,
            &ResumeCatalogRequest {
                sources: vec![SessionSourceKind::NativePi],
                ..ResumeCatalogRequest::default()
            },
        )
        .expect("catalog")
        .rows;
        assert_eq!(
            rows.iter()
                .map(|row| row.session_id.as_str())
                .collect::<Vec<_>>(),
            ["user-row"],
            "workflow-internal rows never reach the selector"
        );
        assert_eq!(rows[0].session_kind, SessionKind::User);
    }

    #[test]
    fn target_and_typed_input_activation_reject_workflow_internal_sessions() {
        let home = tempfile::tempdir().expect("home");
        let catalog = SessionCatalog::new(home.path());
        write_native(&catalog, "user-row", "genuine user prompt");
        write_native_with_kind(&catalog, "supervisor-row", SessionKind::WorkflowSupervisor);
        let supervisor_path = catalog
            .root_for(SessionSourceKind::NativePi)
            .path
            .join("--work--")
            .join("supervisor-row.jsonl");

        let target = resolve_resume_selection(
            &catalog,
            &ResumeSelectionRequest::Target(ResumeSelectorTarget {
                source: SessionSourceKind::NativePi,
                source_path: supervisor_path.clone(),
                status: CatalogRowStatus::Native,
            }),
            None,
            &[SessionSourceKind::NativePi],
        );
        assert!(
            matches!(target, Err(CatalogError::WorkflowInternalSession)),
            "target activation must reject workflow-internal rows: {target:?}"
        );

        let typed = resolve_resume_selection(
            &catalog,
            &ResumeSelectionRequest::Input(supervisor_path.display().to_string()),
            None,
            &[SessionSourceKind::NativePi],
        );
        assert!(
            matches!(typed, Err(CatalogError::WorkflowInternalSession)),
            "typed path activation must reject workflow-internal rows: {typed:?}"
        );

        let user_typed = resolve_resume_selection(
            &catalog,
            &ResumeSelectionRequest::Input(
                catalog
                    .root_for(SessionSourceKind::NativePi)
                    .path
                    .join("--work--")
                    .join("user-row.jsonl")
                    .display()
                    .to_string(),
            ),
            None,
            &[SessionSourceKind::NativePi],
        );
        assert!(
            matches!(&user_typed, Ok(selection) if selection.source == SessionSourceKind::NativePi && selection.native_no_copy),
            "user session activation stays eligible: {user_typed:?}"
        );
    }
}
