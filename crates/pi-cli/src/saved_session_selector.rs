use std::cmp::Ordering;
use std::path::{Path, PathBuf};

use pi_coding::{CatalogRowStatus, SessionSourceKind};

use crate::resume_catalog::{ResumeSelectionRequest, ResumeSelectorRow, ResumeSelectorTarget};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum SessionSort {
    #[default]
    Newest,
    Name,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SessionSelectorMode {
    List,
    Rename { path: PathBuf, value: String },
    ConfirmDelete { path: PathBuf },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SessionManagementRejection {
    NoSelection,
    ForeignSource,
    CurrentSession,
}

impl SessionManagementRejection {
    pub(crate) const fn status_message(self) -> &'static str {
        match self {
            Self::NoSelection => "No session selected",
            Self::ForeignSource => "Foreign source sessions cannot be renamed or deleted",
            Self::CurrentSession => "Cannot delete the currently active session",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SessionManagementResult {
    Started,
    QueryCleared,
    Rejected(SessionManagementRejection),
}

impl SessionManagementResult {
    pub(crate) const fn started(self) -> bool {
        matches!(self, Self::Started)
    }

    pub(crate) const fn status_message(self) -> Option<&'static str> {
        match self {
            Self::Started => None,
            Self::QueryCleared => Some("Cleared session filter; press delete again to confirm"),
            Self::Rejected(reason) => Some(reason.status_message()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SessionSelectorRequest {
    None,
    Resume(ResumeSelectionRequest),
    Rename { path: PathBuf, name: String },
    Delete(PathBuf),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SessionSelectionKey {
    source: SessionSourceKind,
    source_path: PathBuf,
    native_path: Option<PathBuf>,
}

impl SessionSelectionKey {
    fn from_row(row: &ResumeSelectorRow) -> Self {
        Self {
            source: row.source,
            source_path: row.path.clone(),
            native_path: row.native_management_path().map(Path::to_path_buf),
        }
    }

    fn matches_target(&self, row: &ResumeSelectorRow) -> bool {
        self.source == row.source && same_path(&self.source_path, &row.path)
    }

    fn matches_native_path(&self, row: &ResumeSelectorRow) -> bool {
        self.native_path.as_deref().is_some_and(|native_path| {
            row.native_management_path()
                .is_some_and(|row_path| same_path(native_path, row_path))
        })
    }
}

pub(crate) struct SavedSessionSelector {
    rows: Vec<ResumeSelectorRow>,
    visible_indices: Vec<usize>,
    current_path: Option<PathBuf>,
    query: String,
    query_selection: Option<SessionSelectionKey>,
    selected: usize,
    named_only: bool,
    show_path: bool,
    sort: SessionSort,
    mode: SessionSelectorMode,
}

impl SavedSessionSelector {
    pub(crate) fn new(rows: Vec<ResumeSelectorRow>, current_path: Option<PathBuf>) -> Self {
        let mut selector = Self {
            rows,
            visible_indices: Vec::new(),
            current_path: current_path.map(normalized_path),
            query: String::new(),
            query_selection: None,
            selected: 0,
            named_only: false,
            show_path: false,
            sort: SessionSort::Newest,
            mode: SessionSelectorMode::List,
        };
        selector.rebuild_visible_indices();
        selector
    }

    pub(crate) fn reload(&mut self, rows: Vec<ResumeSelectorRow>) {
        let selected = self.selected_row().map(SessionSelectionKey::from_row);
        self.rows = rows;
        self.rebuild_visible_indices();
        self.restore_selection(selected.as_ref());
    }

    pub(crate) fn query(&self) -> &str {
        &self.query
    }

    pub(crate) fn named_only(&self) -> bool {
        self.named_only
    }

    pub(crate) fn show_path(&self) -> bool {
        self.show_path
    }

    pub(crate) fn sort(&self) -> SessionSort {
        self.sort
    }

    pub(crate) fn mode(&self) -> &SessionSelectorMode {
        &self.mode
    }

    pub(crate) fn selected(&self) -> usize {
        self.selected
    }

    pub(crate) fn visible_count(&self) -> usize {
        self.visible_indices.len()
    }

    pub(crate) fn visible_row(&self, index: usize) -> Option<&ResumeSelectorRow> {
        self.visible_indices
            .get(index)
            .and_then(|row_index| self.rows.get(*row_index))
    }

    pub(crate) fn visible_window(
        &self,
        start: usize,
        limit: usize,
    ) -> impl Iterator<Item = (usize, &ResumeSelectorRow)> {
        self.visible_indices
            .iter()
            .enumerate()
            .skip(start)
            .take(limit)
            .map(move |(visible_index, row_index)| (visible_index, &self.rows[*row_index]))
    }

    pub(crate) fn selected_row(&self) -> Option<&ResumeSelectorRow> {
        self.visible_row(self.selected)
    }

    pub(crate) fn is_current(&self, row: &ResumeSelectorRow) -> bool {
        let Some(current) = self.current_path.as_deref() else {
            return false;
        };
        row.native_management_path()
            .is_some_and(|path| same_path(current, path))
    }

    pub(crate) fn push_query(&mut self, character: char) {
        if !matches!(self.mode, SessionSelectorMode::List) {
            return;
        }
        if self.query.is_empty() {
            self.query_selection = self.selected_row().map(SessionSelectionKey::from_row);
        }
        self.query.push(character);
        self.rebuild_visible_indices();
        self.restore_query_selection();
    }

    pub(crate) fn pop_query(&mut self) {
        if !matches!(self.mode, SessionSelectorMode::List) {
            return;
        }
        self.query.pop();
        self.rebuild_visible_indices();
        self.restore_query_selection();
        if self.query.is_empty() {
            self.query_selection = None;
        }
    }

    pub(crate) fn move_selection(&mut self, delta: isize) {
        if !matches!(self.mode, SessionSelectorMode::List) {
            return;
        }
        let count = self.visible_count();
        if count == 0 {
            self.selected = 0;
            return;
        }
        self.selected = (self.selected as isize + delta).rem_euclid(count as isize) as usize;
        if !self.query.is_empty() {
            self.query_selection = self.selected_row().map(SessionSelectionKey::from_row);
        }
    }

    pub(crate) fn toggle_named_filter(&mut self) {
        let selected = self.selected_row().map(SessionSelectionKey::from_row);
        self.named_only = !self.named_only;
        self.rebuild_visible_indices();
        self.restore_selection(selected.as_ref());
    }

    pub(crate) fn toggle_path(&mut self) {
        self.show_path = !self.show_path;
    }

    pub(crate) fn toggle_sort(&mut self) {
        let selected = self.selected_row().map(SessionSelectionKey::from_row);
        self.sort = match self.sort {
            SessionSort::Newest => SessionSort::Name,
            SessionSort::Name => SessionSort::Newest,
        };
        self.rebuild_visible_indices();
        self.restore_selection(selected.as_ref());
    }

    pub(crate) fn begin_rename(&mut self) -> SessionManagementResult {
        let Some(row) = self.selected_row() else {
            return SessionManagementResult::Rejected(SessionManagementRejection::NoSelection);
        };
        let Some(path) = row.native_management_path() else {
            return SessionManagementResult::Rejected(SessionManagementRejection::ForeignSource);
        };
        let path = path.to_path_buf();
        let value = row.name.clone().unwrap_or_default();
        self.mode = SessionSelectorMode::Rename { path, value };
        SessionManagementResult::Started
    }

    pub(crate) fn rename_push(&mut self, character: char) {
        if let SessionSelectorMode::Rename { value, .. } = &mut self.mode {
            value.push(character);
        }
    }

    pub(crate) fn rename_pop(&mut self) {
        if let SessionSelectorMode::Rename { value, .. } = &mut self.mode {
            value.pop();
        }
    }

    pub(crate) fn begin_delete(&mut self, noninvasive: bool) -> SessionManagementResult {
        if noninvasive && !self.query.is_empty() {
            self.pop_query();
            return SessionManagementResult::QueryCleared;
        }
        let Some(row) = self.selected_row() else {
            return SessionManagementResult::Rejected(SessionManagementRejection::NoSelection);
        };
        let Some(path) = row.native_management_path().map(Path::to_path_buf) else {
            return SessionManagementResult::Rejected(SessionManagementRejection::ForeignSource);
        };
        if self.is_current(row) {
            return SessionManagementResult::Rejected(SessionManagementRejection::CurrentSession);
        }
        self.mode = SessionSelectorMode::ConfirmDelete { path };
        SessionManagementResult::Started
    }

    pub(crate) fn cancel_mode(&mut self) -> bool {
        if matches!(self.mode, SessionSelectorMode::List) {
            return false;
        }
        self.mode = SessionSelectorMode::List;
        true
    }

    pub(crate) fn confirm(&mut self) -> SessionSelectorRequest {
        match std::mem::replace(&mut self.mode, SessionSelectorMode::List) {
            SessionSelectorMode::List => {
                self.selected_row()
                    .map_or(SessionSelectorRequest::None, |row| {
                        SessionSelectorRequest::Resume(ResumeSelectionRequest::Target(
                            ResumeSelectorTarget::from(row),
                        ))
                    })
            }
            SessionSelectorMode::Rename { path, value } => {
                let name = value.trim().to_owned();
                if name.is_empty() {
                    SessionSelectorRequest::None
                } else {
                    SessionSelectorRequest::Rename { path, name }
                }
            }
            SessionSelectorMode::ConfirmDelete { path } => SessionSelectorRequest::Delete(path),
        }
    }

    fn rebuild_visible_indices(&mut self) {
        self.visible_indices = (0..self.rows.len())
            .filter(|row_index| {
                let row = &self.rows[*row_index];
                (!self.named_only || has_name(row)) && row_matches(row, &self.query)
            })
            .collect();
        let rows = &self.rows;
        match self.sort {
            SessionSort::Newest => self.visible_indices.sort_by(|left_index, right_index| {
                let left = &rows[*left_index];
                let right = &rows[*right_index];
                right
                    .modified_epoch
                    .total_cmp(&left.modified_epoch)
                    .then_with(|| compare_row_identity(left, right))
            }),
            SessionSort::Name => self.visible_indices.sort_by(|left_index, right_index| {
                let left = &rows[*left_index];
                let right = &rows[*right_index];
                case_insensitive_cmp(session_display_name(left), session_display_name(right))
                    .then_with(|| right.modified_epoch.total_cmp(&left.modified_epoch))
                    .then_with(|| compare_row_identity(left, right))
            }),
        }
        self.selected = self.selected.min(self.visible_count().saturating_sub(1));
    }

    fn restore_query_selection(&mut self) {
        self.restore_selection(self.query_selection.clone().as_ref());
    }

    fn restore_selection(&mut self, selection: Option<&SessionSelectionKey>) {
        self.selected = selection
            .and_then(|selection| {
                self.visible_window(0, self.visible_count())
                    .find(|(_, row)| selection.matches_target(row))
                    .map(|(index, _)| index)
                    .or_else(|| {
                        self.visible_window(0, self.visible_count())
                            .find(|(_, row)| selection.matches_native_path(row))
                            .map(|(index, _)| index)
                    })
            })
            .unwrap_or(0);
    }
}

pub(crate) fn session_display_name(row: &ResumeSelectorRow) -> &str {
    row.name
        .as_deref()
        .filter(|name| !name.trim().is_empty())
        .or_else(|| (!row.summary.trim().is_empty()).then_some(row.summary.as_str()))
        .unwrap_or(&row.session_id)
}

fn has_name(row: &ResumeSelectorRow) -> bool {
    row.name
        .as_ref()
        .is_some_and(|name| !name.trim().is_empty())
}

fn row_matches(row: &ResumeSelectorRow, query: &str) -> bool {
    if query.trim().is_empty() {
        return true;
    }
    let cwd = row.cwd.to_string_lossy();
    let path = row.path.to_string_lossy();
    query.split_whitespace().all(|token| {
        [
            row.source.as_str(),
            row.source_badge,
            row.session_id.as_str(),
            row.name.as_deref().unwrap_or_default(),
            row.summary.as_str(),
            cwd.as_ref(),
            path.as_ref(),
            row.message_blob.as_str(),
        ]
        .into_iter()
        .any(|candidate| fuzzy_match(candidate, token))
    })
}

fn fuzzy_match(candidate: &str, query: &str) -> bool {
    let mut candidate = candidate.chars().flat_map(char::to_lowercase);
    query
        .chars()
        .flat_map(char::to_lowercase)
        .all(|expected| candidate.by_ref().any(|actual| actual == expected))
}

fn compare_row_identity(left: &ResumeSelectorRow, right: &ResumeSelectorRow) -> Ordering {
    left.source
        .as_str()
        .cmp(right.source.as_str())
        .then_with(|| left.session_id.cmp(&right.session_id))
        .then_with(|| left.name.cmp(&right.name))
        .then_with(|| left.summary.cmp(&right.summary))
        .then_with(|| left.cwd.cmp(&right.cwd))
        .then_with(|| left.path.cmp(&right.path))
        .then_with(|| left.search_text.cmp(&right.search_text))
}

fn case_insensitive_cmp(left: &str, right: &str) -> Ordering {
    left.chars()
        .flat_map(char::to_lowercase)
        .cmp(right.chars().flat_map(char::to_lowercase))
}

fn same_path(left: &Path, right: &Path) -> bool {
    left == right || normalized_path(left) == normalized_path(right)
}

fn normalized_path(path: impl AsRef<Path>) -> PathBuf {
    std::fs::canonicalize(path.as_ref()).unwrap_or_else(|_| path.as_ref().to_path_buf())
}

trait ResumeSelectorRowExt {
    fn native_management_path(&self) -> Option<&Path>;
}

impl ResumeSelectorRowExt for ResumeSelectorRow {
    fn native_management_path(&self) -> Option<&Path> {
        match &self.status {
            CatalogRowStatus::Native => Some(&self.path),
            CatalogRowStatus::AlreadyImported { native_path, .. } => Some(native_path),
            CatalogRowStatus::Foreign => None,
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn row(
        source: SessionSourceKind,
        id: &str,
        name: Option<&str>,
        summary: &str,
        cwd: &str,
        path: &str,
        modified_epoch: f64,
        status: CatalogRowStatus,
        search_text: &str,
        message_blob: &str,
    ) -> ResumeSelectorRow {
        ResumeSelectorRow {
            source,
            source_badge: source.label(),
            session_id: id.to_owned(),
            cwd: PathBuf::from(cwd),
            modified_epoch,
            display_time: format!("{modified_epoch}"),
            summary: summary.to_owned(),
            path: PathBuf::from(path),
            size: 1,
            message_count: Some(2),
            name: name.map(str::to_owned),
            status,
            search_text: search_text.to_owned(),
            message_blob: message_blob.to_owned(),
        }
    }

    fn mixed_rows() -> Vec<ResumeSelectorRow> {
        vec![
            row(
                SessionSourceKind::NativePi,
                "pi-id",
                Some("Zulu Native"),
                "pi summary",
                "/work/pi",
                "/sessions/pi.jsonl",
                30.0,
                CatalogRowStatus::Native,
                "pi-id Zulu Native pi summary /work/pi /sessions/pi.jsonl",
                "pi summary",
            ),
            row(
                SessionSourceKind::Codex,
                "codex-id",
                None,
                "Codex summary",
                "/work/codex",
                "/foreign/codex.jsonl",
                20.0,
                CatalogRowStatus::Foreign,
                "codex-id Codex summary deeply buried needle",
                "deeply buried needle",
            ),
            row(
                SessionSourceKind::Claude,
                "claude-id",
                Some("Alpha Imported"),
                "Claude summary",
                "/work/claude",
                "/foreign/claude.jsonl",
                10.0,
                CatalogRowStatus::AlreadyImported {
                    native_id: "native-claude".to_owned(),
                    native_path: PathBuf::from("/sessions/imported-claude.jsonl"),
                },
                "claude-id Alpha Imported Claude summary imported transcript",
                "imported transcript",
            ),
        ]
    }

    fn ids_for_query(rows: &[ResumeSelectorRow], query: &str) -> Vec<String> {
        let mut selector = SavedSessionSelector::new(rows.to_vec(), None);
        for character in query.chars() {
            selector.push_query(character);
        }
        selector
            .visible_window(0, selector.visible_count())
            .map(|(_, row)| row.session_id.clone())
            .collect()
    }

    #[test]
    fn mixed_rows_retain_badges_status_and_filter_all_local_corpus_fields() {
        let rows = mixed_rows();
        let selector = SavedSessionSelector::new(rows.clone(), None);
        let visible = selector
            .visible_window(0, selector.visible_count())
            .map(|(_, row)| row)
            .collect::<Vec<_>>();
        assert_eq!(
            visible
                .iter()
                .map(|row| (row.source_badge, row.session_id.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("pi", "pi-id"),
                ("codex", "codex-id"),
                ("claude", "claude-id")
            ]
        );
        assert_eq!(visible[0].status, CatalogRowStatus::Native);
        assert_eq!(visible[1].status, CatalogRowStatus::Foreign);
        assert!(matches!(
            visible[2].status,
            CatalogRowStatus::AlreadyImported { .. }
        ));

        for (query, expected) in [
            ("cdx", "codex-id"),
            ("cldid", "claude-id"),
            ("alpimp", "claude-id"),
            ("cdxsum", "codex-id"),
            ("wrkcld", "claude-id"),
            ("frgcdx", "codex-id"),
            ("dpbrndl", "codex-id"),
        ] {
            assert_eq!(ids_for_query(&rows, query), vec![expected], "query {query}");
        }
    }

    #[test]
    fn row_matches_does_not_bridge_filter_through_search_text() {
        // `search_text` concatenates every field (including cwd/path), so
        // matching against it let a query span field boundaries the selector
        // should not see. row_matches now matches the isolated message_blob,
        // so a token present only in search_text must not match — while tokens
        // in message_blob and the explicit cwd candidate still do.
        let rows = vec![row(
            SessionSourceKind::Codex,
            "codex-id",
            None,
            "Codex summary",
            "/work/codex",
            "/foreign/codex.jsonl",
            20.0,
            CatalogRowStatus::Foreign,
            "codex-id Codex summary zxqvbridge-only-in-search /work/codex /foreign/codex.jsonl",
            "actual message content",
        )];
        // Token present ONLY in search_text -> must not match after the fix.
        assert!(
            ids_for_query(&rows, "zxqvbridge").is_empty(),
            "row_matches must not bridge via search_text; only message_blob and explicit fields are matchable"
        );
        // Sanity: message_blob content still matches.
        assert_eq!(ids_for_query(&rows, "actual"), vec!["codex-id"]);
        // Sanity: cwd remains an explicit matchable candidate.
        assert_eq!(ids_for_query(&rows, "wrkcdx"), vec!["codex-id"]);
    }

    #[test]
    fn sorting_filtering_and_reload_preserve_stable_target_or_native_path() {
        let rows = mixed_rows();
        let mut selector = SavedSessionSelector::new(rows.clone(), None);
        selector.move_selection(2);
        assert_eq!(selector.selected_row().unwrap().session_id, "claude-id");

        selector.toggle_sort();
        assert_eq!(selector.sort(), SessionSort::Name);
        assert_eq!(selector.selected_row().unwrap().session_id, "claude-id");

        for character in "cld".chars() {
            selector.push_query(character);
        }
        assert_eq!(selector.selected_row().unwrap().session_id, "claude-id");
        while !selector.query().is_empty() {
            selector.pop_query();
        }
        assert_eq!(selector.selected_row().unwrap().session_id, "claude-id");

        let mut reloaded = rows;
        let imported = reloaded
            .iter_mut()
            .find(|row| row.source == SessionSourceKind::Claude)
            .unwrap();
        imported.path = PathBuf::from("/foreign/moved-claude.jsonl");
        imported.session_id = "claude-moved".to_owned();
        selector.reload(reloaded);
        assert_eq!(selector.selected_row().unwrap().session_id, "claude-moved");
    }

    #[test]
    fn window_views_are_bounded_and_navigation_wraps() {
        let rows = (0..32)
            .map(|index| {
                let id = format!("session-{index:02}");
                let path = format!("/sessions/{id}.jsonl");
                row(
                    SessionSourceKind::NativePi,
                    &id,
                    None,
                    &id,
                    "/work",
                    &path,
                    f64::from(u32::try_from(index).expect("fixture index")),
                    CatalogRowStatus::Native,
                    &id,
                    &id,
                )
            })
            .collect();
        let mut selector = SavedSessionSelector::new(rows, None);
        assert_eq!(
            selector
                .visible_count(), 32);
        assert_eq!(
            selector
                .visible_window(3, 4)
                .map(|(index, row)| (index, row.session_id.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (3, "session-28"),
                (4, "session-27"),
                (5, "session-26"),
                (6, "session-25"),
            ]
        );
        selector.move_selection(isize::MAX);
        assert_eq!(
            selector
                .selected(), 31);
        assert_eq!(selector.selected_row().unwrap().session_id, "session-00");
        // Wrapping down from the last row returns to the first.
        selector.move_selection(1);
        assert_eq!(
            selector
                .selected(), 0);
        assert_eq!(selector.selected_row().unwrap().session_id, "session-31");
    }

    #[test]
    fn move_selection_wraps_around_visible_sessions() {
        let mut selector = SavedSessionSelector::new(mixed_rows(), None);
        // Visible order (newest first): pi-id, codex-id, claude-id.
        // Down past the end returns to the first row.
        selector.move_selection(3);
        assert_eq!(selector.selected(), 0);
        assert_eq!(selector.selected_row().unwrap().session_id, "pi-id");
        selector.move_selection(2);
        assert_eq!(selector.selected(), 2);
        selector.move_selection(1);
        assert_eq!(selector.selected(), 0);
        // Up from the first row wraps to the last.
        selector.move_selection(-1);
        assert_eq!(selector.selected(), 2);
        assert_eq!(selector.selected_row().unwrap().session_id, "claude-id");
        // A single-row list stays put in both directions.
        let mut single = SavedSessionSelector::new(vec![mixed_rows().remove(0)], None);
        single.move_selection(5);
        assert_eq!(single.selected(), 0);
        single.move_selection(-5);
        assert_eq!(single.selected(), 0);
    }

    #[test]
    fn move_selection_wraps_within_filtered_visible_sessions() {
        let mut selector = SavedSessionSelector::new(mixed_rows(), None);
        selector.push_query('c');
        // Filtered visible (newest first): codex-id, claude-id.
        assert_eq!(
            selector
                .visible_window(0, selector.visible_count())
                .map(|(_, row)| row.session_id.as_str())
                .collect::<Vec<_>>(),
            vec!["codex-id", "claude-id"]
        );
        selector.move_selection(1);
        assert_eq!(selector.selected_row().unwrap().session_id, "claude-id");
        // Down past the end of the filtered list wraps back to its first row.
        selector.move_selection(1);
        assert_eq!(selector.selected_row().unwrap().session_id, "codex-id");
        // Up from the filtered list's first row wraps to its last row.
        selector.move_selection(-1);
        assert_eq!(selector.selected_row().unwrap().session_id, "claude-id");
    }

    #[test]
    fn confirm_emits_target_without_losing_source_or_status() {
        let mut selector = SavedSessionSelector::new(mixed_rows(), None);
        selector.move_selection(1);
        assert_eq!(
            selector.confirm(),
            SessionSelectorRequest::Resume(ResumeSelectionRequest::Target(ResumeSelectorTarget {
                source: SessionSourceKind::Codex,
                source_path: PathBuf::from("/foreign/codex.jsonl"),
                status: CatalogRowStatus::Foreign,
            }))
        );
    }

    #[test]
    fn foreign_rows_reject_management_without_emitting_a_source_path_request() {
        let foreign = mixed_rows().remove(1);
        let mut selector = SavedSessionSelector::new(vec![foreign], None);
        for result in [selector.begin_rename(), selector.begin_delete(false)] {
            assert_eq!(
                result,
                SessionManagementResult::Rejected(SessionManagementRejection::ForeignSource)
            );
            assert_eq!(
                result.status_message(),
                Some("Foreign source sessions cannot be renamed or deleted")
            );
            assert_eq!(selector.mode(), &SessionSelectorMode::List);
        }
        assert!(matches!(
            selector.confirm(),
            SessionSelectorRequest::Resume(ResumeSelectionRequest::Target(_))
        ));
    }

    #[test]
    fn already_imported_management_uses_native_path_and_current_guard() {
        let imported = mixed_rows().remove(2);
        let native_path = PathBuf::from("/sessions/imported-claude.jsonl");
        let mut selector = SavedSessionSelector::new(vec![imported.clone()], None);
        assert_eq!(selector.begin_rename(), SessionManagementResult::Started);
        selector.rename_push('!');
        assert_eq!(
            selector.confirm(),
            SessionSelectorRequest::Rename {
                path: native_path.clone(),
                name: "Alpha Imported!".to_owned(),
            }
        );
        assert_eq!(
            selector.begin_delete(false),
            SessionManagementResult::Started
        );
        assert_eq!(
            selector.confirm(),
            SessionSelectorRequest::Delete(native_path.clone())
        );

        let mut current = SavedSessionSelector::new(vec![imported], Some(native_path));
        assert_eq!(
            current.begin_delete(false),
            SessionManagementResult::Rejected(SessionManagementRejection::CurrentSession)
        );
    }

    #[test]
    fn native_management_and_noninvasive_delete_use_native_path() {
        let native = mixed_rows().remove(0);
        let mut selector = SavedSessionSelector::new(vec![native], None);
        selector.push_query('p');
        assert_eq!(
            selector.begin_delete(true),
            SessionManagementResult::QueryCleared
        );
        assert!(selector.query().is_empty());
        assert_eq!(
            selector.begin_delete(false),
            SessionManagementResult::Started
        );
        assert_eq!(
            selector.confirm(),
            SessionSelectorRequest::Delete(PathBuf::from("/sessions/pi.jsonl"))
        );
    }
}
