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
        let path = catalog
            .root_for(SessionSourceKind::NativePi)
            .path
            .join("--work--")
            .join(format!("{id}.jsonl"));
        fs::create_dir_all(path.parent().expect("native parent")).expect("native parent");
        fs::write(
            &path,
            format!(
                "{{\"type\":\"session\",\"version\":3,\"id\":\"{id}\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"cwd\":\"/tmp/work\"}}\n{{\"type\":\"message\",\"id\":\"u\",\"parentId\":null,\"message\":{{\"role\":\"user\",\"content\":\"{summary}\"}}}}\n"
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
