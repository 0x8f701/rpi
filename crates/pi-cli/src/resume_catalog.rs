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
    CatalogSearchOptions, CatalogSort, ResolvedSession, SessionCatalog, SessionSourceKind,
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

const WEB_DEFAULT_RESUME_SOURCES: [SessionSourceKind; 4] = [
    SessionSourceKind::NativePi,
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
}

/// List/search/sort the unified catalog for a selector.
pub fn load_resume_catalog(
    catalog: &SessionCatalog,
    request: &ResumeCatalogRequest,
) -> Result<ResumeCatalogResult, CatalogError> {
    let sources = normalize_resume_sources(&request.sources);
    let rows = match request.query.as_deref().map(str::trim).filter(|query| !query.is_empty()) {
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

/// Web catalog rows prefer the native imported copy over its foreign source.
/// The source remains discoverable until import; after import the native row
/// is the only actionable representation in the Web control plane.
#[must_use]
pub fn coalesce_web_import_rows(rows: Vec<ResumeSelectorRow>) -> Vec<ResumeSelectorRow> {
    let native_paths = rows
        .iter()
        .filter(|row| row.source.is_native())
        .map(|row| row.path.clone())
        .collect::<std::collections::HashSet<_>>();
    rows.into_iter()
        .filter(|row| match &row.status {
            CatalogRowStatus::AlreadyImported { native_path, .. } => {
                !native_paths.contains(native_path)
            }
            CatalogRowStatus::Native | CatalogRowStatus::Foreign => true,
        })
        .collect()
}

const WEB_NOISE_SESSION_MAX_BYTES: u64 = 10 * 1024;

/// Hide small persisted rows that contain no user/assistant messages from the
/// Web catalog. The size guard matches the product's 10 KiB noise threshold,
/// while the zero-message requirement preserves short but recoverable turns.
/// Temp-workspace rows are NOT dropped here: they are flagged `temporary` by
/// [`partition_web_noise_rows`] so the sidebar can hide them by default
/// without losing them (searchable, and loaded/active sessions stay visible).
#[must_use]
pub fn filter_web_noise_rows(rows: Vec<ResumeSelectorRow>) -> Vec<ResumeSelectorRow> {
    rows.into_iter()
        .filter(|row| {
            row.size >= WEB_NOISE_SESSION_MAX_BYTES || row.message_count != Some(0)
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
            CatalogRowStatus::Native => Ok(ResumeSelectionResult {
                source: SessionSourceKind::NativePi,
                path: target.source_path.clone(),
                imported: false,
                reused_existing: true,
                native_no_copy: true,
                message_count: None,
                cancelled: false,
            }),
            CatalogRowStatus::AlreadyImported { native_path, .. } => {
                ensure_source_enabled(target.source, &sources, &target.source_path)?;
                Ok(ResumeSelectionResult {
                    source: target.source,
                    path: native_path.clone(),
                    imported: false,
                    reused_existing: true,
                    native_no_copy: false,
                    message_count: None,
                    cancelled: false,
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
                return Ok(ResumeSelectionResult {
                    source: kind,
                    path,
                    imported: false,
                    reused_existing: true,
                    native_no_copy: true,
                    message_count: None,
                    cancelled: false,
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
    Ok(ResumeSelectionResult { cancelled: outcome.cancelled, ..result })
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
            catalog.is_safe_session_path(*kind, &candidate)
                || candidate.starts_with(catalog.root_for(*kind).path)
        });
        if let Some(kind) = classified {
            ensure_source_enabled(kind, sources, &candidate)?;
            return catalog.resolve_for(kind, &candidate).map(|path| (kind, path));
        }
        if candidate.extension() == Some(OsStr::new("jsonl")) {
            return catalog
                .resolve_for(SessionSourceKind::NativePi, &candidate)
                .map(|path| (SessionSourceKind::NativePi, path));
        }
        return Err(CatalogError::SessionNotFound(candidate.display().to_string()));
    }

    let mut matches = Vec::new();
    for kind in sources {
        match catalog.resolve_for(*kind, input_path) {
            Ok(path) => matches.push((*kind, path)),
            Err(CatalogError::SessionNotFound(_)) => {}
            Err(CatalogError::AmbiguousSession { matches: ambiguous, .. }) => {
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

    #[test]
    fn web_default_sources_include_common_local_agents() {
        assert_eq!(
            web_resume_sources_from_settings(&pi_coding::Settings::default()),
            [
                SessionSourceKind::NativePi,
                SessionSourceKind::Omp,
                SessionSourceKind::Codex,
                SessionSourceKind::Grok,
            ]
        );
    }

    #[test]
    fn web_explicit_empty_sources_remain_native_only() {
        let settings: pi_coding::Settings = serde_json::from_str(
            r#"{"sessionImportSources":[]}"#,
        )
        .expect("explicit empty sources");
        assert_eq!(
            web_resume_sources_from_settings(&settings),
            [SessionSourceKind::NativePi]
        );
    }

    #[test]
    fn web_explicit_sources_replace_the_fallback() {
        let settings: pi_coding::Settings = serde_json::from_str(
            r#"{"sessionImportSources":["codex"]}"#,
        )
        .expect("explicit Codex source");
        assert_eq!(
            web_resume_sources_from_settings(&settings),
            [SessionSourceKind::NativePi, SessionSourceKind::Codex]
        );
    }

    #[test]
    fn web_catalog_prefers_imported_native_row_over_foreign_lineage_row() {
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

        let web_rows = coalesce_web_import_rows(rows);
        assert_eq!(web_rows.len(), 1);
        assert_eq!(web_rows[0].source, SessionSourceKind::NativePi);
        assert!(matches!(web_rows[0].status, CatalogRowStatus::Native));
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
        assert!(visible.iter().any(|row| row.size == 10_240 && row.message_count == Some(0)));
        assert!(visible.iter().any(|row| row.message_count == Some(1)));
        assert!(visible.iter().any(|row| row.message_count.is_none()));
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
        assert!(temporary.is_empty(), "non-temp short rows are never temporary");
    }

    #[test]
    fn web_noise_partition_keeps_named_temp_rows_regular() {
        let temp = temp_workspace_cwd();
        let rows = vec![
            noise_row(
                SessionSourceKind::NativePi,
                &temp,
                512,
                Some(1),
                Some("my experiment"),
            ),
        ];
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
        assert!(regular.iter().all(|row| row.size >= WEB_NOISE_SESSION_MAX_BYTES));
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
        write_native_with_cwd(
            &catalog,
            "user-short",
            "hello there",
            &real_workspace_cwd(),
        );
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
        assert!(matches!(row.status, CatalogRowStatus::AlreadyImported { .. }));
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

    #[test]
    fn foreign_import_uses_exact_native_session_root() {
        let home = tempfile::tempdir().expect("home");
        let native = tempfile::tempdir().expect("native root");
        let catalog = SessionCatalog::new(home.path())
            .with_native_session_root(native.path());
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

}
