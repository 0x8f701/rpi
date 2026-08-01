//! Product-surface adapter for the unified native and foreign session catalog.
//!
//! This module deliberately contains no terminal rendering. The line REPL,
//! startup argument handling, and a later TUI integration all consume the same
//! request/row/selection types and selection semantics.

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
    /// Empty means every source.
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
            sources: Vec::new(),
            include_foreign: true,
            dedupe: true,
            named_only: false,
            cwd_scope: None,
            sort: CatalogSort::Newest,
        }
    }
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
}

/// List/search/sort the unified catalog for a selector.
pub fn load_resume_catalog(
    catalog: &SessionCatalog,
    request: &ResumeCatalogRequest,
) -> Result<ResumeCatalogResult, CatalogError> {
    let rows = match request.query.as_deref().map(str::trim).filter(|query| !query.is_empty()) {
        Some(query) => catalog.search(
            query,
            &CatalogSearchOptions {
                sources: request.sources.clone(),
                include_foreign: request.include_foreign,
                dedupe: request.dedupe,
                named_only: request.named_only,
                cwd_scope: request.cwd_scope.clone(),
            },
        )?,
        None => catalog.list(&CatalogListOptions {
            sources: request.sources.clone(),
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
/// Native rows and `AlreadyImported` rows are returned directly. A genuinely
/// foreign row is passed through the catalog's idempotent import path.
pub fn resolve_resume_selection(
    catalog: &SessionCatalog,
    request: &ResumeSelectionRequest,
    preferred_cwd: Option<&Path>,
) -> Result<ResumeSelectionResult, CatalogError> {
    match request {
        ResumeSelectionRequest::Target(target) => match &target.status {
            CatalogRowStatus::Native => Ok(ResumeSelectionResult {
                source: SessionSourceKind::NativePi,
                path: target.source_path.clone(),
                imported: false,
                reused_existing: true,
                native_no_copy: true,
                message_count: None,
            }),
            CatalogRowStatus::AlreadyImported { native_path, .. } => Ok(ResumeSelectionResult {
                source: target.source,
                path: native_path.clone(),
                imported: false,
                reused_existing: true,
                native_no_copy: false,
                message_count: None,
            }),
            CatalogRowStatus::Foreign => imported_result(catalog.import_or_resume(
                target.source,
                &target.source_path,
                preferred_cwd,
            )?),
        },
        ResumeSelectionRequest::Input(input) => {
            let (kind, path) = catalog.resolve_any(input)?;
            if kind.is_native() {
                return Ok(ResumeSelectionResult {
                    source: kind,
                    path,
                    imported: false,
                    reused_existing: true,
                    native_no_copy: true,
                    message_count: None,
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
) -> Result<ResumeSelectionResult> {
    let result = resolve_resume_selection(catalog, request, preferred_cwd)
        .context("resolving resume selection")?;
    application
        .switch_session(&result.path)
        .await
        .with_context(|| format!("switching to session {}", result.path.display()))?;
    Ok(result)
}

fn imported_result(resolved: ResolvedSession) -> Result<ResumeSelectionResult, CatalogError> {
    Ok(ResumeSelectionResult {
        source: resolved.kind,
        path: resolved.path,
        imported: !resolved.reused_existing,
        reused_existing: resolved.reused_existing,
        native_no_copy: resolved.native_no_copy,
        message_count: Some(resolved.messages.len()),
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
        )
        .expect("first import");
        assert!(first.imported);

        let rows = load_resume_catalog(&catalog, &ResumeCatalogRequest::default())
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
        )
        .expect("direct existing selection");
        assert_eq!(second.path, first.path);
        assert!(!second.imported);
        assert!(second.reused_existing);
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
        )
        .expect("exact id");
        assert_eq!(exact.path, exact_path);

        let error = resolve_resume_selection(
            &catalog,
            &ResumeSelectionRequest::Input("shared-".to_owned()),
            None,
        )
        .expect_err("ambiguous prefix");
        assert!(matches!(error, CatalogError::AmbiguousSession { .. }));
    }

}
