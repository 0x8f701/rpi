use crate::keybindings::Action;
use pi_ai::{ContentBlock, Message};
use pi_coding::{SessionEntry, SessionTreeNode, SessionTreeResult};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TreePanelMode {
    Navigate,
    Fork,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TreeFilterMode {
    Default,
    NoTools,
    UserOnly,
    LabeledOnly,
    All,
}
impl TreeFilterMode {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::NoTools => "no tools",
            Self::UserOnly => "user only",
            Self::LabeledOnly => "labeled only",
            Self::All => "all",
        }
    }
    fn next(self) -> Self {
        match self {
            Self::Default => Self::NoTools,
            Self::NoTools => Self::UserOnly,
            Self::UserOnly => Self::LabeledOnly,
            Self::LabeledOnly => Self::All,
            Self::All => Self::Default,
        }
    }
    fn previous(self) -> Self {
        match self {
            Self::Default => Self::All,
            Self::NoTools => Self::Default,
            Self::UserOnly => Self::NoTools,
            Self::LabeledOnly => Self::UserOnly,
            Self::All => Self::LabeledOnly,
        }
    }
}
#[derive(Clone)]
struct Node {
    entry: SessionEntry,
    label: Option<String>,
    label_timestamp: Option<String>,
    parent: Option<String>,
    children: Vec<String>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct VisibleTreeNode {
    pub(crate) id: String,
    pub(crate) depth: usize,
    pub(crate) gutters: Vec<bool>,
    pub(crate) is_last: bool,
    pub(crate) selected: bool,
    pub(crate) active: bool,
    pub(crate) foldable: bool,
    pub(crate) folded: bool,
    pub(crate) label: Option<String>,
    pub(crate) label_timestamp: Option<String>,
    pub(crate) text: String,
}
pub(crate) struct TreePanel {
    pub(crate) title: String,
    pub(crate) mode: TreePanelMode,
    nodes: HashMap<String, Node>,
    roots: Vec<String>,
    active: HashSet<String>,
    ordered: Vec<String>,
    visible: Vec<VisibleTreeNode>,
    pub(crate) selected: usize,
    pub(crate) filter: TreeFilterMode,
    pub(crate) query: String,
    pub(crate) folded: HashSet<String>,
    pub(crate) show_label_timestamps: bool,
    pub(crate) horizontal_offset: usize,
    pub(crate) label_input: Option<String>,
}
impl TreePanel {
    pub(crate) fn new(result: SessionTreeResult, mode: TreePanelMode) -> Self {
        let mut nodes = HashMap::new();
        let mut roots = vec![];
        for root in result.tree {
            roots.push(root.entry.id.clone());
            insert(root, None, &mut nodes)
        }
        let active = active_path(result.active_leaf_id.as_deref(), &nodes);
        let mut p = Self {
            title: if mode == TreePanelMode::Navigate { "Session Tree".into() } else { "Fork from Message".into() },
            mode,
            nodes,
            roots,
            active,
            ordered: vec![],
            visible: vec![],
            selected: 0,
            filter: if mode == TreePanelMode::Fork { TreeFilterMode::UserOnly } else { TreeFilterMode::Default },
            query: String::new(),
            folded: HashSet::new(),
            show_label_timestamps: false,
            horizontal_offset: 0,
            label_input: None,
        };
        p.rebuild();
        if mode == TreePanelMode::Fork {
            p.selected = p.visible.len().saturating_sub(1);
            p.mark();
        } else if let Some(i) = p.visible.iter().position(|n| n.active) {
            p.selected = i;
            p.mark()
        }
        p
    }
    pub(crate) fn visible(&self) -> &[VisibleTreeNode] {
        &self.visible
    }
    pub(crate) fn selected_entry(&self) -> Option<&SessionEntry> {
        self.visible
            .get(self.selected)
            .and_then(|v| self.nodes.get(&v.id))
            .map(|n| &n.entry)
    }
    pub(crate) fn selected_id(&self) -> Option<&str> {
        self.visible.get(self.selected).map(|n| n.id.as_str())
    }
    pub(crate) fn selected_label(&self) -> Option<&str> {
        self.visible
            .get(self.selected)
            .and_then(|v| self.nodes.get(&v.id))
            .and_then(|n| n.label.as_deref())
    }
    pub(crate) fn selected_is_forkable(&self) -> bool {
        self.selected_entry().is_some_and(is_user)
    }
    pub(crate) fn begin_label_edit(&mut self) {
        self.label_input = Some(self.selected_label().unwrap_or_default().into())
    }
    pub(crate) fn finish_label_edit(&mut self) -> Option<(String, Option<String>)> {
        let id = self.selected_id()?.to_owned();
        let s = self.label_input.take()?;
        let s = s.trim();
        Some((id, (!s.is_empty()).then(|| s.to_owned())))
    }
    pub(crate) fn cancel_label_edit(&mut self) {
        self.label_input = None
    }
    pub(crate) fn insert_label_char(&mut self, c: char) {
        if let Some(s) = &mut self.label_input {
            s.push(c)
        }
    }
    pub(crate) fn backspace_label(&mut self) {
        if let Some(s) = &mut self.label_input {
            s.pop();
        }
    }
    pub(crate) fn insert_search_char(&mut self, c: char) {
        self.query.push(c);
        self.folded.clear();
        self.rebuild()
    }
    pub(crate) fn backspace_search(&mut self) {
        self.query.pop();
        self.folded.clear();
        self.rebuild()
    }
    pub(crate) fn clear_search_or_folds(&mut self) -> bool {
        if self.query.is_empty() && self.folded.is_empty() {
            false
        } else {
            self.query.clear();
            self.folded.clear();
            self.rebuild();
            true
        }
    }
    pub(crate) fn apply_action(&mut self, a: Action, page: usize) -> bool {
        if self.mode == TreePanelMode::Fork {
            match a {
                Action::EditorUp => self.mv(-1),
                Action::EditorDown => self.mv(1),
                Action::EditorPageUp => self.mv(-(page.max(1) as isize)),
                Action::EditorPageDown => self.mv(page.max(1) as isize),
                _ => return false,
            }
            return true;
        }
        match a {
            Action::EditorUp => self.mv(-1),
            Action::EditorDown => self.mv(1),
            Action::EditorPageUp => self.mv(-(page.max(1) as isize)),
            Action::EditorPageDown => self.mv(page.max(1) as isize),
            Action::EditorLeft | Action::TreeFoldOrUp => self.fold_or_up(),
            Action::EditorRight | Action::TreeUnfoldOrDown => self.unfold_or_down(),
            Action::TreeEditLabel => self.begin_label_edit(),
            Action::TreeToggleLabelTimestamp => self.show_label_timestamps = !self.show_label_timestamps,
            Action::TreeFilterDefault => self.set_filter(TreeFilterMode::Default, false),
            Action::TreeFilterNoTools => self.set_filter(TreeFilterMode::NoTools, true),
            Action::TreeFilterUserOnly => self.set_filter(TreeFilterMode::UserOnly, true),
            Action::TreeFilterLabeledOnly => self.set_filter(TreeFilterMode::LabeledOnly, true),
            Action::TreeFilterAll => self.set_filter(TreeFilterMode::All, true),
            Action::TreeFilterCycleForward => { self.filter = self.filter.next(); self.folded.clear(); self.rebuild() }
            Action::TreeFilterCycleBackward => { self.filter = self.filter.previous(); self.folded.clear(); self.rebuild() }
            _ => return false,
        }
        true
    }
    fn set_filter(&mut self, f: TreeFilterMode, t: bool) {
        self.filter = if t && self.filter == f {
            TreeFilterMode::Default
        } else {
            f
        };
        self.folded.clear();
        self.rebuild()
    }
    fn mv(&mut self, d: isize) {
        if self.visible.is_empty() {
            return;
        }
        self.selected =
            (self.selected as isize + d).rem_euclid(self.visible.len() as isize) as usize;
        self.mark()
    }
    fn fold_or_up(&mut self) {
        let Some(id) = self.selected_id().map(str::to_owned) else {
            return;
        };
        if self.foldable(&id) && !self.folded.contains(&id) {
            self.folded.insert(id);
            self.rebuild()
        } else {
            self.mv(-1)
        }
    }
    fn unfold_or_down(&mut self) {
        let Some(id) = self.selected_id().map(str::to_owned) else {
            return;
        };
        if self.folded.remove(&id) {
            self.rebuild()
        } else {
            self.mv(1)
        }
    }
    fn foldable(&self, id: &str) -> bool {
        self.nodes
            .get(id)
            .is_some_and(|n| n.children.iter().any(|c| self.subtree_visible(c)))
    }
    fn subtree_visible(&self, id: &str) -> bool {
        self.nodes
            .get(id)
            .is_some_and(|n| self.matches(n) || n.children.iter().any(|c| self.subtree_visible(c)))
    }
    fn rebuild(&mut self) {
        let selected = self.selected_id().map(str::to_owned);
        self.ordered.clear();
        let mut roots = self.roots.clone();
        self.sort_active(&mut roots);
        for r in roots {
            self.flatten(&r)
        }
        let ids = self
            .ordered
            .iter()
            .filter(|id| {
                self.nodes.get(*id).is_some_and(|n| self.matches(n)) && !self.folded_ancestor(id)
            })
            .cloned()
            .collect::<Vec<_>>();
        let set = ids.iter().cloned().collect::<HashSet<_>>();
        let mut parent = HashMap::new();
        for id in &ids {
            let mut p = self.nodes[id].parent.clone();
            while let Some(x) = p.clone() {
                if set.contains(&x) {
                    break;
                }
                p = self.nodes[&x].parent.clone()
            }
            parent.insert(id.clone(), p);
        }
        let mut children: HashMap<Option<String>, Vec<String>> = HashMap::new();
        for id in &ids {
            children
                .entry(parent[id].clone())
                .or_default()
                .push(id.clone())
        }
        self.visible = ids
            .iter()
            .map(|id| {
                let n = &self.nodes[id];
                let mut ancestors = vec![];
                let mut p = parent[id].clone();
                while let Some(x) = p {
                    ancestors.push(x.clone());
                    p = parent[&x].clone()
                }
                ancestors.reverse();
                let gutters = ancestors
                    .iter()
                    .map(|a| {
                        children
                            .get(&parent[a])
                            .and_then(|s| s.last())
                            .is_some_and(|last| last != a)
                    })
                    .collect();
                let is_last = children
                    .get(&parent[id])
                    .and_then(|s| s.last())
                    .is_some_and(|last| last == id);
                VisibleTreeNode {
                    id: id.clone(),
                    depth: ancestors.len(),
                    gutters,
                    is_last,
                    selected: false,
                    active: self.active.contains(id),
                    foldable: self.foldable(id),
                    folded: self.folded.contains(id),
                    label: n.label.clone(),
                    label_timestamp: n.label_timestamp.clone(),
                    text: entry_text(&n.entry),
                }
            })
            .collect();
        self.selected = selected
            .as_deref()
            .and_then(|id| self.visible.iter().position(|n| n.id == id))
            .unwrap_or_else(|| self.selected.min(self.visible.len().saturating_sub(1)));
        self.mark()
    }
    fn mark(&mut self) {
        for (i, n) in self.visible.iter_mut().enumerate() {
            n.selected = i == self.selected
        }
    }
    fn flatten(&mut self, id: &str) {
        self.ordered.push(id.into());
        if self.folded.contains(id) {
            return;
        }
        let mut c = self.nodes[id].children.clone();
        self.sort_active(&mut c);
        for id in c {
            self.flatten(&id)
        }
    }
    fn sort_active(&self, ids: &mut [String]) {
        ids.sort_by(|l, r| {
            self.active
                .contains(r)
                .cmp(&self.active.contains(l))
                .then_with(|| {
                    self.nodes[l]
                        .entry
                        .timestamp
                        .cmp(&self.nodes[r].entry.timestamp)
                })
        })
    }
    fn folded_ancestor(&self, id: &str) -> bool {
        let mut p = self.nodes[id].parent.as_deref();
        while let Some(x) = p {
            if self.folded.contains(x) {
                return true;
            }
            p = self.nodes[x].parent.as_deref()
        }
        false
    }
    fn matches(&self, n: &Node) -> bool {
        let ok = match self.filter {
            TreeFilterMode::Default => !matches!(
                n.entry.entry_type.as_str(),
                "label" | "custom" | "model_change" | "thinking_level_change" | "session_info"
            ),
            TreeFilterMode::NoTools => {
                !matches!(
                    n.entry.entry_type.as_str(),
                    "label" | "custom" | "model_change" | "thinking_level_change" | "session_info"
                ) && !matches!(n.entry.message.as_ref(), Some(Message::ToolResult(_)))
            }
            TreeFilterMode::UserOnly => is_user(&n.entry),
            TreeFilterMode::LabeledOnly => n.label.is_some(),
            TreeFilterMode::All => true,
        };
        ok && (self.query.is_empty()
            || fuzzy(&entry_text(&n.entry), &self.query)
            || n.label.as_deref().is_some_and(|l| fuzzy(l, &self.query)))
    }
}
fn insert(t: SessionTreeNode, p: Option<String>, m: &mut HashMap<String, Node>) {
    let id = t.entry.id.clone();
    let children = t.children.iter().map(|child| child.entry.id.clone()).collect();
    let nested = t.children;
    m.insert(id.clone(), Node { entry: t.entry, label: t.label, label_timestamp: t.label_timestamp, parent: p, children });
    for child in nested { insert(child, Some(id.clone()), m); }
}
fn active_path(leaf: Option<&str>, nodes: &HashMap<String, Node>) -> HashSet<String> {
    let mut path = HashSet::new();
    let mut cursor = leaf;
    while let Some(id) = cursor {
        if !path.insert(id.to_owned()) { break; }
        cursor = nodes.get(id).and_then(|node| node.parent.as_deref());
    }
    path
}
fn is_user(entry: &SessionEntry) -> bool {
    matches!(entry.message.as_ref(), Some(Message::User(_)))
}
fn entry_text(e: &SessionEntry) -> String {
    match e.entry_type.as_str() {
        "message" => match e.message.as_ref() {
            Some(Message::User(m)) => format!("user: {}", text(&m.content)),
            Some(Message::Assistant(m)) => format!("assistant: {}", m.text()),
            Some(Message::ToolResult(m)) => format!("[tool: {}] {}", m.tool_name, text(&m.content)),
            Some(Message::BashExecution(m)) => format!("[bash] {}", m.command),
            Some(Message::Custom(m)) => format!("custom: {}", text(&m.content.to_blocks())),
            Some(Message::BranchSummary(m)) => format!("[branch summary] {}", m.summary),
            Some(Message::CompactionSummary(m)) => format!("[compaction] {}", m.summary),
            None => "message".into(),
        },
        "branch_summary" => format!(
            "[branch summary] {}",
            e.summary.as_deref().unwrap_or_default()
        ),
        "compaction" => format!("[compaction] {}", e.summary.as_deref().unwrap_or_default()),
        "model_change" => format!(
            "[model: {}/{}]",
            e.provider.as_deref().unwrap_or_default(),
            e.model_id.as_deref().unwrap_or_default()
        ),
        "thinking_level_change" => format!(
            "[thinking: {}]",
            e.thinking_level.as_deref().unwrap_or_default()
        ),
        "session_info" => format!("[title: {}]", e.name.as_deref().unwrap_or_default()),
        other => format!("[{other}]"),
    }
}
fn text(c: &[ContentBlock]) -> String {
    c.iter()
        .filter_map(|b| {
            if let ContentBlock::Text { text, .. } = b {
                Some(text.as_str())
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}
fn fuzzy(v: &str, q: &str) -> bool {
    let v = v.to_lowercase();
    let mut c = v.chars();
    q.to_lowercase().chars().all(|n| c.by_ref().any(|x| x == n))
}
#[cfg(test)]
mod tests {
    use super::*;
    fn e(id: &str, p: Option<&str>, s: &str) -> SessionEntry {
        SessionEntry {
            entry_type: "message".into(),
            id: id.into(),
            parent_id: p.map(str::to_owned),
            timestamp: id.into(),
            message: Some(Message::user_text(s, 0)),
            provider: None,
            model_id: None,
            thinking_level: None,
            summary: None,
            first_kept_entry_id: None,
            tokens_before: None,
            retained_tail: vec![],
            content: None,
            display: None,
            details: None,
            data: None,
            name: None,
            label: None,
            target_id: None,
            from_id: None,
            custom_type: None,
            todo_state: None,
        }
    }
    fn n(e: SessionEntry, c: Vec<SessionTreeNode>) -> SessionTreeNode {
        SessionTreeNode {
            entry: e,
            children: c,
            label: None,
            label_timestamp: None,
        }
    }
    #[test]
    fn active_first() {
        let p = TreePanel::new(
            SessionTreeResult {
                tree: vec![
                    n(
                        e("1", None, "r"),
                        vec![
                            n(e("2", Some("1"), "i"), vec![]),
                            n(e("3", Some("1"), "a"), vec![]),
                        ],
                    ),
                    n(e("4", None, "o"), vec![]),
                ],
                leaf_id: Some("4".into()),
                active_leaf_id: Some("3".into()),
            },
            TreePanelMode::Navigate,
        );
        assert_eq!(
            p.visible()
                .iter()
                .map(|n| n.id.as_str())
                .collect::<Vec<_>>(),
            ["1", "3", "2", "4"]
        )
    }
    #[test]
    fn fork_uses_original_user_message_list_and_starts_at_latest() {
        let mut assistant = e("2", Some("1"), "assistant");
        assistant.message = Some(Message::Assistant(pi_ai::AssistantMessage::pending(&pi_ai::Model::default())));
        let panel = TreePanel::new(
            SessionTreeResult {
                tree: vec![n(e("1", None, "older"), vec![n(assistant, vec![]), n(e("3", Some("1"), "latest"), vec![])])],
                leaf_id: Some("3".into()),
                active_leaf_id: Some("3".into()),
            },
            TreePanelMode::Fork,
        );
        assert_eq!(panel.title, "Fork from Message");
        assert_eq!(panel.visible().iter().map(|node| node.id.as_str()).collect::<Vec<_>>(), ["1", "3"]);
        assert_eq!(panel.selected_id(), Some("3"));
        assert!(panel.selected_is_forkable());
    }
    #[test]
    fn filters_search_fold_and_label_timestamp_are_stateful() {
        let mut labeled = n(e("2", Some("1"), "needle"), vec![]);
        labeled.label = Some("checkpoint".into());
        labeled.label_timestamp = Some("now".into());
        let mut p = TreePanel::new(
            SessionTreeResult {
                tree: vec![n(e("1", None, "root"), vec![labeled])],
                leaf_id: Some("2".into()),
                active_leaf_id: Some("2".into()),
            },
            TreePanelMode::Navigate,
        );
        p.apply_action(Action::TreeFilterLabeledOnly, 10);
        assert_eq!(p.visible().len(), 1);
        p.insert_search_char('c');
        p.insert_search_char('p');
        assert_eq!(p.visible()[0].id, "2");
        p.apply_action(Action::TreeToggleLabelTimestamp, 10);
        assert!(p.show_label_timestamps);
        p.clear_search_or_folds();
        p.apply_action(Action::TreeFilterDefault, 10);
        p.apply_action(Action::EditorLeft, 10);
        p.apply_action(Action::EditorLeft, 10);
        assert_eq!(p.visible().len(), 1);
        p.apply_action(Action::EditorRight, 10);
        assert_eq!(p.visible().len(), 2)
    }
}
