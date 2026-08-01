use std::path::{Path, PathBuf};

use pi_coding::SessionInfo;

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

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SessionSelectorRequest {
    None,
    Resume(PathBuf),
    Rename { path: PathBuf, name: String },
    Delete(PathBuf),
}

pub(crate) struct SavedSessionSelector {
    sessions: Vec<SessionInfo>,
    current_path: Option<PathBuf>,
    query: String,
    query_selection_path: Option<PathBuf>,
    selected: usize,
    named_only: bool,
    show_path: bool,
    sort: SessionSort,
    mode: SessionSelectorMode,
}

impl SavedSessionSelector {
    pub(crate) fn new(sessions: Vec<SessionInfo>, current_path: Option<PathBuf>) -> Self {
        Self {
            sessions,
            current_path: current_path.map(normalized_path),
            query: String::new(),
            query_selection_path: None,
            selected: 0,
            named_only: false,
            show_path: false,
            sort: SessionSort::Newest,
            mode: SessionSelectorMode::List,
        }
    }

    pub(crate) fn reload(&mut self, sessions: Vec<SessionInfo>) {
        let selected_path = self.selected_session().map(|session| session.path.clone());
        self.sessions = sessions;
        self.selected = selected_path
            .as_ref()
            .and_then(|path| {
                self.visible_sessions()
                    .iter()
                    .position(|session| session.path == *path)
            })
            .unwrap_or(0);
        self.clamp_selection();
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

    pub(crate) fn visible_sessions(&self) -> Vec<&SessionInfo> {
        let mut sessions = self
            .sessions
            .iter()
            .filter(|session| !self.named_only || has_name(session))
            .filter(|session| session_matches(session, &self.query))
            .collect::<Vec<_>>();
        match self.sort {
            SessionSort::Newest => sessions.sort_by(|left, right| {
                right
                    .timestamp
                    .cmp(&left.timestamp)
                    .then_with(|| left.id.cmp(&right.id))
            }),
            SessionSort::Name => sessions.sort_by(|left, right| {
                session_display_name(left)
                    .to_lowercase()
                    .cmp(&session_display_name(right).to_lowercase())
                    .then_with(|| right.timestamp.cmp(&left.timestamp))
                    .then_with(|| left.id.cmp(&right.id))
            }),
        }
        sessions
    }

    pub(crate) fn selected_session(&self) -> Option<&SessionInfo> {
        self.visible_sessions().get(self.selected).copied()
    }

    pub(crate) fn is_current(&self, session: &SessionInfo) -> bool {
        self.current_path
            .as_ref()
            .is_some_and(|current| *current == normalized_path(&session.path))
    }

    pub(crate) fn push_query(&mut self, character: char) {
        if !matches!(self.mode, SessionSelectorMode::List) {
            return;
        }
        if self.query.is_empty() {
            self.query_selection_path = self.selected_session().map(|session| session.path.clone());
        }
        self.query.push(character);
        self.restore_query_selection();
    }

    pub(crate) fn pop_query(&mut self) {
        if !matches!(self.mode, SessionSelectorMode::List) {
            return;
        }
        self.query.pop();
        self.restore_query_selection();
        if self.query.is_empty() {
            self.query_selection_path = None;
        }
    }

    pub(crate) fn move_selection(&mut self, delta: isize) {
        if !matches!(self.mode, SessionSelectorMode::List) {
            return;
        }
        let count = self.visible_sessions().len();
        if count == 0 {
            self.selected = 0;
            return;
        }
        self.selected = self.selected.saturating_add_signed(delta).min(count - 1);
        if !self.query.is_empty() {
            self.query_selection_path = self.selected_session().map(|session| session.path.clone());
        }
    }
    pub(crate) fn toggle_named_filter(&mut self) {
        self.named_only = !self.named_only;
        self.selected = 0;
    }

    pub(crate) fn toggle_path(&mut self) {
        self.show_path = !self.show_path;
    }

    pub(crate) fn toggle_sort(&mut self) {
        self.sort = match self.sort {
            SessionSort::Newest => SessionSort::Name,
            SessionSort::Name => SessionSort::Newest,
        };
        self.selected = 0;
    }

    pub(crate) fn begin_rename(&mut self) {
        let Some(session) = self.selected_session() else {
            return;
        };
        self.mode = SessionSelectorMode::Rename {
            path: session.path.clone(),
            value: session.name.clone().unwrap_or_default(),
        };
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

    pub(crate) fn begin_delete(&mut self, noninvasive: bool) -> bool {
        if noninvasive && !self.query.is_empty() {
            self.pop_query();
            return false;
        }
        let Some(session) = self.selected_session() else {
            return false;
        };
        if self.is_current(session) {
            return false;
        }
        self.mode = SessionSelectorMode::ConfirmDelete {
            path: session.path.clone(),
        };
        true
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
            SessionSelectorMode::List => self
                .selected_session()
                .map_or(SessionSelectorRequest::None, |session| {
                    SessionSelectorRequest::Resume(session.path.clone())
                }),
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

    fn restore_query_selection(&mut self) {
        self.selected = self
            .query_selection_path
            .as_ref()
            .and_then(|path| {
                self.visible_sessions()
                    .iter()
                    .position(|session| session.path == *path)
            })
            .unwrap_or(0);
    }

    fn clamp_selection(&mut self) {
        self.selected = self
            .selected
            .min(self.visible_sessions().len().saturating_sub(1));
    }
}

pub(crate) fn session_display_name(session: &SessionInfo) -> &str {
    session
        .name
        .as_deref()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or(&session.first_message)
}

fn has_name(session: &SessionInfo) -> bool {
    session
        .name
        .as_ref()
        .is_some_and(|name| !name.trim().is_empty())
}

fn session_matches(session: &SessionInfo, query: &str) -> bool {
    if query.trim().is_empty() {
        return true;
    }
    let search = format!(
        "{} {} {} {} {}",
        session.id,
        session.name.as_deref().unwrap_or_default(),
        session.all_messages_text,
        session.cwd.display(),
        session.path.display()
    );
    query
        .split_whitespace()
        .all(|token| fuzzy_match(&search, token))
}

fn fuzzy_match(candidate: &str, query: &str) -> bool {
    let mut candidate = candidate.chars().flat_map(char::to_lowercase);
    query
        .chars()
        .flat_map(char::to_lowercase)
        .all(|expected| candidate.by_ref().any(|actual| actual == expected))
}

fn normalized_path(path: impl AsRef<Path>) -> PathBuf {
    std::fs::canonicalize(path.as_ref()).unwrap_or_else(|_| path.as_ref().to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str, name: Option<&str>, timestamp: &str) -> SessionInfo {
        SessionInfo {
            path: PathBuf::from(format!("/sessions/{id}.jsonl")),
            id: id.to_owned(),
            cwd: PathBuf::from("/workspace"),
            timestamp: timestamp.to_owned(),
            messages: 1,
            name: name.map(str::to_owned),
            first_message: id.to_owned(),
            all_messages_text: id.to_owned(),
        }
    }

    #[test]
    fn filters_named_and_fuzzy_fields_then_toggles_sort() {
        let mut selector = SavedSessionSelector::new(
            vec![
                session("older", Some("Zulu Project"), "2026-01-01"),
                session("newer", Some("Alpha Project"), "2026-01-03"),
                session("plain", None, "2026-01-04"),
            ],
            None,
        );
        assert_eq!(
            selector
                .visible_sessions()
                .iter()
                .map(|session| session.id.as_str())
                .collect::<Vec<_>>(),
            vec!["plain", "newer", "older"]
        );
        selector.toggle_named_filter();
        assert_eq!(
            selector
                .visible_sessions()
                .iter()
                .map(|session| session.id.as_str())
                .collect::<Vec<_>>(),
            vec!["newer", "older"]
        );
        selector.push_query('z');
        selector.push_query('p');
        assert_eq!(selector.visible_sessions()[0].id, "older");
        selector.pop_query();
        selector.pop_query();
        selector.toggle_sort();
        assert_eq!(
            selector
                .visible_sessions()
                .iter()
                .map(|session| session.id.as_str())
                .collect::<Vec<_>>(),
            vec!["newer", "older"]
        );
    }

    #[test]
    fn path_toggle_rename_and_delete_modes_are_noninvasive_and_safe_for_current() {
        let current = PathBuf::from("/sessions/current.jsonl");
        let mut selector = SavedSessionSelector::new(
            vec![
                session("current", Some("Current"), "2026-01-03"),
                session("other", Some("Old"), "2026-01-02"),
            ],
            Some(current),
        );
        selector.toggle_path();
        assert!(selector.show_path());
        assert!(!selector.begin_delete(false));
        selector.move_selection(1);
        selector.push_query('x');
        assert!(!selector.begin_delete(true));
        assert!(selector.query().is_empty());
        selector.begin_rename();
        selector.rename_push('!');
        assert_eq!(
            selector.confirm(),
            SessionSelectorRequest::Rename {
                path: PathBuf::from("/sessions/other.jsonl"),
                name: "Old!".to_owned()
            }
        );
        assert!(selector.begin_delete(false));
        assert_eq!(
            selector.confirm(),
            SessionSelectorRequest::Delete(PathBuf::from("/sessions/other.jsonl"))
        );
    }
}
