use crate::keybindings::Action;
use pi_ai::{ContentBlock, Message};
use pi_coding::{SessionEntry, SessionTreeNode, SessionTreeResult};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::rc::Rc;

/// Maximum width (in characters) of a node label in the session tree.
/// Labels longer than this are truncated with an ellipsis; the renderer
/// additionally truncates the composed row to the panel's inner width so
/// a row can never wrap or overflow the panel.
const NODE_LABEL_MAX: usize = 60;

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
/// Compact per-row branch metadata, shared by every visible row of one
/// rebuild: the effective (nearest visible) parent and whether later
/// siblings follow the row. Walking `parent` from a row visits its
/// ancestors; their `has_more` flags are exactly the row's gutter bits.
struct RowMeta {
    parent: Option<u32>,
    has_more: bool,
}
/// Gutter state of a visible row: a shared reference into the rebuild's
/// row metadata plus the row's position. Iterating yields one bool per
/// visible ancestor, outermost first, in O(depth) time without storing
/// O(depth) bits per row — every row of a chain shares one metadata array.
#[derive(Clone)]
pub(crate) struct Gutters {
    rows: Rc<[RowMeta]>,
    row: u32,
}
pub(crate) struct GutterIter<'a> {
    ancestors: Vec<&'a RowMeta>,
    pos: usize,
}
impl Gutters {
    fn iter(&self) -> GutterIter<'_> {
        let mut ancestors = Vec::new();
        let mut cursor = self.row as usize;
        while let Some(p) = self.rows[cursor].parent {
            ancestors.push(&self.rows[p as usize]);
            cursor = p as usize;
        }
        ancestors.reverse();
        GutterIter { ancestors, pos: 0 }
    }
}
impl<'a> IntoIterator for &'a Gutters {
    type Item = &'a bool;
    type IntoIter = GutterIter<'a>;
    fn into_iter(self) -> GutterIter<'a> {
        self.iter()
    }
}
impl<'a> Iterator for GutterIter<'a> {
    type Item = &'a bool;
    fn next(&mut self) -> Option<&'a bool> {
        let meta = *self.ancestors.get(self.pos)?;
        self.pos += 1;
        Some(&meta.has_more)
    }
}
impl PartialEq for Gutters {
    fn eq(&self, other: &Self) -> bool {
        (Rc::ptr_eq(&self.rows, &other.rows) && self.row == other.row)
            || self.iter().eq(other.iter())
    }
}
impl Eq for Gutters {}
impl fmt::Debug for Gutters {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Gutters").field("row", &self.row).finish()
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct VisibleTreeNode {
    pub(crate) id: String,
    pub(crate) depth: usize,
    pub(crate) gutters: Gutters,
    pub(crate) is_last: bool,
    pub(crate) selected: bool,
    pub(crate) active: bool,
    pub(crate) foldable: bool,
    pub(crate) folded: bool,
    pub(crate) label: Option<String>,
    pub(crate) label_timestamp: Option<String>,
    /// Single-line, width-bounded display label derived from the entry.
    pub(crate) label_text: String,
}
/// Compact row data produced by the single DFS pass in `rebuild`.
struct Row {
    id: String,
    depth: usize,
    parent_row: Option<u32>,
    active: bool,
    folded: bool,
    foldable: bool,
    label: Option<String>,
    label_timestamp: Option<String>,
    label_text: String,
}
/// One node on the DFS stack: its pending child index, whether any node on
/// the path from the root to it (inclusive) is folded, its match flag, and
/// the OR accumulator that turns into the row's `foldable` on exit. `depth`
/// is the tree depth, so sibling root ascend can never leak into the next
/// root's accumulator as if the prior subtree were its child.
struct DfsFrame {
    id: String,
    next_child: usize,
    depth: usize,
    folded_path: bool,
    matched: bool,
    below: bool,
    row: Option<u32>,
    should_emit: bool,
    unvisited: bool,
}
enum DfsStep {
    Descend(String, bool, usize),
    Ascend,
}
pub(crate) struct TreePanel {
    pub(crate) title: String,
    pub(crate) mode: TreePanelMode,
    nodes: HashMap<String, Node>,
    roots: Vec<String>,
    active: HashSet<String>,
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
    pub(crate) fn new(mut result: SessionTreeResult, mode: TreePanelMode) -> Self {
        let mut nodes = HashMap::new();
        let mut roots = vec![];
        let tree = std::mem::take(&mut result.tree);
        for root in tree {
            roots.push(root.entry.id.clone());
            insert(root, None, &mut nodes)
        }
        let active = active_path(result.active_leaf_id.as_deref(), &nodes);
        let mut p = Self {
            title: if mode == TreePanelMode::Navigate {
                "Session Tree".into()
            } else {
                "Fork from Message".into()
            },
            mode,
            nodes,
            roots,
            active,
            visible: vec![],
            selected: 0,
            filter: if mode == TreePanelMode::Fork {
                TreeFilterMode::UserOnly
            } else {
                TreeFilterMode::Default
            },
            query: String::new(),
            folded: HashSet::new(),
            show_label_timestamps: false,
            horizontal_offset: 0,
            label_input: None,
        };
        // Display order (active path first, then by timestamp) depends only
        // on the fixed active set and entry timestamps, so sort the roots and
        // child lists exactly once instead of on every rebuild.
        sort_active(&p.active, &p.nodes, &mut p.roots);
        let ids: Vec<String> = p.nodes.keys().cloned().collect();
        for id in &ids {
            let mut children = std::mem::take(&mut p.nodes.get_mut(id).unwrap().children);
            sort_active(&p.active, &p.nodes, &mut children);
            p.nodes.get_mut(id).unwrap().children = children;
        }
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
            Action::TreeToggleLabelTimestamp => {
                self.show_label_timestamps = !self.show_label_timestamps
            }
            Action::TreeFilterDefault => self.set_filter(TreeFilterMode::Default, false),
            Action::TreeFilterNoTools => self.set_filter(TreeFilterMode::NoTools, true),
            Action::TreeFilterUserOnly => self.set_filter(TreeFilterMode::UserOnly, true),
            Action::TreeFilterLabeledOnly => self.set_filter(TreeFilterMode::LabeledOnly, true),
            Action::TreeFilterAll => self.set_filter(TreeFilterMode::All, true),
            Action::TreeFilterCycleForward => {
                self.filter = self.filter.next();
                self.folded.clear();
                self.rebuild()
            }
            Action::TreeFilterCycleBackward => {
                self.filter = self.filter.previous();
                self.folded.clear();
                self.rebuild()
            }
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
        let foldable = self
            .visible
            .get(self.selected)
            .is_some_and(|row| row.foldable);
        if foldable && !self.folded.contains(&id) {
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
    fn is_hidden_message(&self, node: &Node) -> bool {
        matches!(node.entry.message.as_ref(), Some(Message::Custom(message)) if !message.display)
            || (node.entry.entry_type == "custom_message" && node.entry.display == Some(false))
    }

    fn matches(&self, n: &Node) -> bool {
        if self.is_hidden_message(n) {
            return false;
        }
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
    /// Rebuild the visible rows in one iterative DFS. Each node is visited
    /// exactly once: its match flag is computed on entry (also feeding the
    /// foldable accumulators unwound on exit), the row is emitted when it
    /// matches and no ancestor is folded, and the ancestor stack yields the
    /// effective parent and depth in O(1). No ancestor chains, repeated
    /// root walks or per-row O(depth) gutters are materialized.
    fn rebuild(&mut self) {
        let selected = self.selected_id().map(str::to_owned);
        let mut rows: Vec<Row> = Vec::with_capacity(self.nodes.len());
        let mut anc_rows: Vec<u32> = Vec::new();
        let mut stack: Vec<DfsFrame> = Vec::new();
        // Roots are pushed in reverse so the LIFO stack visits the first
        // root's whole subtree (pre-order) before the next root.
        for id in self.roots.iter().rev() {
            let matched = self.matches(&self.nodes[id]);
            let folded_path = self.folded.contains(id);
            stack.push(DfsFrame {
                id: id.clone(),
                next_child: 0,
                depth: 0,
                folded_path,
                matched,
                below: false,
                row: None,
                should_emit: matched,
                unvisited: true,
            });
        }
        loop {
            // Emit the top frame's row on first visit: pre-order rows, so a
            // root's whole subtree is emitted before the next root.
            if let Some(frame) = stack.last_mut() {
                if frame.unvisited {
                    frame.unvisited = false;
                    if frame.should_emit {
                        let node = &self.nodes[&frame.id];
                        rows.push(Row {
                            id: frame.id.clone(),
                            depth: anc_rows.len(),
                            parent_row: anc_rows.last().copied(),
                            active: self.active.contains(&frame.id),
                            folded: self.folded.contains(&frame.id),
                            foldable: false,
                            label: node.label.clone(),
                            label_timestamp: node.label_timestamp.clone(),
                            label_text: node_label(&node.entry),
                        });
                        frame.row = Some((rows.len() - 1) as u32);
                        anc_rows.push((rows.len() - 1) as u32);
                    }
                }
            }
            let step = match stack.last_mut() {
                None => break,
                Some(frame) => {
                    let children = self.nodes[&frame.id].children.as_slice();
                    match children.get(frame.next_child) {
                        Some(child_id) => {
                            let child_id = child_id.clone();
                            frame.next_child += 1;
                            DfsStep::Descend(child_id, frame.folded_path, frame.depth)
                        }
                        None => DfsStep::Ascend,
                    }
                }
            };
            match step {
                DfsStep::Descend(child_id, parent_folded_path, parent_depth) => {
                    let child = &self.nodes[&child_id];
                    let matched = self.matches(child);
                    let folded_path = parent_folded_path || self.folded.contains(&child_id);
                    stack.push(DfsFrame {
                        id: child_id,
                        next_child: 0,
                        depth: parent_depth + 1,
                        folded_path,
                        matched,
                        below: false,
                        row: None,
                        should_emit: !parent_folded_path && matched,
                        unvisited: true,
                    });
                }
                DfsStep::Ascend => {
                    let frame = stack.pop().expect("checked non-empty");
                    if frame.depth > 0
                        && let Some(parent) = stack.last_mut()
                    {
                        parent.below |= frame.matched || frame.below;
                    }
                    if let Some(r) = frame.row {
                        rows[r as usize].foldable = frame.below;
                        anc_rows.pop();
                    }
                }
            }
        }
        // A row is the last child of its effective parent iff no later row
        // shares that parent; sibling groups interleave with deeper rows, so
        // scan from the right.
        let mut seen: HashSet<Option<u32>> = HashSet::with_capacity(rows.len());
        let mut metas: Vec<RowMeta> = rows
            .iter()
            .rev()
            .map(|row| RowMeta {
                parent: row.parent_row,
                has_more: !seen.insert(row.parent_row),
            })
            .collect();
        metas.reverse();
        let metas: Rc<[RowMeta]> = metas.into();
        self.visible = rows
            .into_iter()
            .enumerate()
            .map(|(i, row)| VisibleTreeNode {
                id: row.id,
                depth: row.depth,
                gutters: Gutters {
                    rows: Rc::clone(&metas),
                    row: i as u32,
                },
                is_last: !metas[i].has_more,
                selected: false,
                active: row.active,
                foldable: row.foldable,
                folded: row.folded,
                label: row.label,
                label_timestamp: row.label_timestamp,
                label_text: row.label_text,
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
}
fn insert(t: SessionTreeNode, p: Option<String>, m: &mut HashMap<String, Node>) {
    let mut pending: Vec<(SessionTreeNode, Option<String>)> = vec![(t, p)];
    while let Some((node, parent)) = pending.pop() {
        let id = node.entry.id.clone();
        let nested = node.children;
        let children = nested.iter().map(|child| child.entry.id.clone()).collect();
        m.insert(
            id.clone(),
            Node {
                entry: node.entry,
                label: node.label,
                label_timestamp: node.label_timestamp,
                parent,
                children,
            },
        );
        for child in nested {
            pending.push((child, Some(id.clone())));
        }
    }
}
fn active_path(leaf: Option<&str>, nodes: &HashMap<String, Node>) -> HashSet<String> {
    let mut path = HashSet::new();
    let mut cursor = leaf;
    while let Some(id) = cursor {
        if !path.insert(id.to_owned()) {
            break;
        }
        cursor = nodes.get(id).and_then(|node| node.parent.as_deref());
    }
    path
}
/// Order sibling entries so the active path comes first, then by timestamp.
/// The sort only depends on the fixed active set and entry timestamps, so it
/// runs once per panel instead of on every rebuild.
fn sort_active(active: &HashSet<String>, nodes: &HashMap<String, Node>, ids: &mut [String]) {
    ids.sort_by(|l, r| {
        active
            .contains(r)
            .cmp(&active.contains(l))
            .then_with(|| nodes[l].entry.timestamp.cmp(&nodes[r].entry.timestamp))
    })
}
fn is_user(entry: &SessionEntry) -> bool {
    matches!(entry.message.as_ref(), Some(Message::User(_)))
}
/// The type prefix and full (possibly multi-line) body of an entry.
/// The body carries the exact separator the prefix expects, so
/// `format!("{prefix}{body}")` reproduces the full entry text verbatim.
fn entry_parts(e: &SessionEntry) -> (String, String) {
    match e.entry_type.as_str() {
        "message" => match e.message.as_ref() {
            Some(Message::User(m)) => ("user: ".into(), text(&m.content)),
            Some(Message::Assistant(m)) => ("assistant: ".into(), m.text()),
            Some(Message::ToolResult(m)) => (
                format!("[tool: {}]", m.tool_name),
                format!(" {}", text(&m.content)),
            ),
            Some(Message::BashExecution(m)) => ("[bash] ".into(), m.command.clone()),
            Some(Message::Custom(m)) => ("custom: ".into(), text(&m.content.to_blocks())),
            Some(Message::BranchSummary(m)) => ("[branch summary] ".into(), m.summary.clone()),
            Some(Message::CompactionSummary(m)) => ("[compaction] ".into(), m.summary.clone()),
            None => ("message".into(), String::new()),
        },
        "branch_summary" => (
            "[branch summary] ".into(),
            e.summary.clone().unwrap_or_default(),
        ),
        "compaction" => (
            "[compaction] ".into(),
            e.summary.clone().unwrap_or_default(),
        ),
        "model_change" => (
            format!(
                "[model: {}/{}]",
                e.provider.as_deref().unwrap_or_default(),
                e.model_id.as_deref().unwrap_or_default()
            ),
            String::new(),
        ),
        "thinking_level_change" => (
            format!(
                "[thinking: {}]",
                e.thinking_level.as_deref().unwrap_or_default()
            ),
            String::new(),
        ),
        "session_info" => (
            format!("[title: {}]", e.name.as_deref().unwrap_or_default()),
            String::new(),
        ),
        other => (format!("[{other}]"), String::new()),
    }
}

/// Full text of an entry (type prefix + complete body, possibly multi-line).
/// Used only for search matching; never displayed directly.
fn entry_text(e: &SessionEntry) -> String {
    let (prefix, body) = entry_parts(e);
    format!("{prefix}{body}")
}

/// Single-line, width-bounded label used for tree display: the entry type
/// (user/assistant/tool/custom/...) plus a preview of the first line,
/// truncated to `NODE_LABEL_MAX` characters with an ellipsis. Tool results
/// render as just the short `[tool: <name>]` tag so their output never
/// floods the tree.
fn node_label(e: &SessionEntry) -> String {
    let (prefix, body) = entry_parts(e);
    let preview = if matches!(e.message.as_ref(), Some(Message::ToolResult(_))) {
        ""
    } else {
        first_line(&body)
    };
    let composed = if preview.is_empty() {
        prefix
    } else {
        format!("{prefix}{preview}")
    };
    truncate_label(composed.trim_end())
}

/// The first line of `s` (everything before the first CR or LF).
fn first_line(s: &str) -> &str {
    match s.find(['\r', '\n']) {
        Some(i) => &s[..i],
        None => s,
    }
}

/// Truncate `s` to `NODE_LABEL_MAX` characters, appending an ellipsis when
/// content was cut.
fn truncate_label(s: &str) -> String {
    if s.chars().count() <= NODE_LABEL_MAX {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(NODE_LABEL_MAX - 1).collect();
    out.push('…');
    out
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
            usage: None,
            from_hook: None,
            data: None,
            name: None,
            label: None,
            target_id: None,
            from_id: None,
            custom_type: None,
            todo_state: None,
            todo_execution_status: None,
            todo_failure_reason: None,
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
        );
        assert!(
            !p.visible()[3].foldable,
            "a childless sibling root must not inherit the prior root subtree's below accumulator"
        );
    }
    #[test]
    fn fork_uses_original_user_message_list_and_starts_at_latest() {
        let mut assistant = e("2", Some("1"), "assistant");
        assistant.message = Some(Message::Assistant(pi_ai::AssistantMessage::pending(
            &pi_ai::Model::default(),
        )));
        let panel = TreePanel::new(
            SessionTreeResult {
                tree: vec![n(
                    e("1", None, "older"),
                    vec![n(assistant, vec![]), n(e("3", Some("1"), "latest"), vec![])],
                )],
                leaf_id: Some("3".into()),
                active_leaf_id: Some("3".into()),
            },
            TreePanelMode::Fork,
        );
        assert_eq!(panel.title, "Fork from Message");
        assert_eq!(
            panel
                .visible()
                .iter()
                .map(|node| node.id.as_str())
                .collect::<Vec<_>>(),
            ["1", "3"]
        );
        assert_eq!(panel.selected_id(), Some("3"));
        assert!(panel.selected_is_forkable());
    }
    #[test]
    fn hidden_custom_messages_never_appear_in_tree_selectors() {
        let mut hidden = e("2", Some("1"), "internal");
        hidden.message = Some(Message::Custom(pi_ai::CustomMessage {
            custom_type: pi_coding::LOOP_SCHEDULED_MESSAGE_TYPE.to_owned(),
            content: "<system-reminder>internal</system-reminder>".into(),
            display: false,
            details: None,
            timestamp: 1,
        }));
        let mut panel = TreePanel::new(
            SessionTreeResult {
                tree: vec![n(e("1", None, "visible"), vec![n(hidden, vec![])])],
                leaf_id: Some("2".into()),
                active_leaf_id: Some("2".into()),
            },
            TreePanelMode::Navigate,
        );
        assert_eq!(
            panel
                .visible()
                .iter()
                .map(|node| node.id.as_str())
                .collect::<Vec<_>>(),
            ["1"]
        );
        panel.apply_action(Action::TreeFilterAll, 10);
        assert_eq!(
            panel
                .visible()
                .iter()
                .map(|node| node.id.as_str())
                .collect::<Vec<_>>(),
            ["1"]
        );
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

    #[test]
    fn long_multi_line_assistant_message_renders_as_single_truncated_label() {
        let mut assistant = e("1", None, "short");
        let long_first_line =
            "the quick brown fox jumps over the lazy dog and then some more words ".repeat(3);
        assistant.message = Some(Message::Assistant(pi_ai::AssistantMessage::pending(
            &pi_ai::Model::default(),
        )));
        if let Some(Message::Assistant(m)) = &mut assistant.message {
            m.content = vec![pi_ai::ContentBlock::text(format!(
                "{long_first_line}\nsecond line that must never appear"
            ))];
        }
        let panel = TreePanel::new(
            SessionTreeResult {
                tree: vec![n(assistant, vec![])],
                leaf_id: None,
                active_leaf_id: None,
            },
            TreePanelMode::Navigate,
        );
        let label = &panel.visible()[0].label_text;
        assert!(
            label.starts_with("assistant: "),
            "label must keep its type prefix: {label:?}"
        );
        assert!(
            !label.contains('\n'),
            "label must be a single line: {label:?}"
        );
        assert!(
            label.chars().count() <= NODE_LABEL_MAX,
            "label must respect the cap: {label:?}"
        );
        assert!(
            label.ends_with('…'),
            "truncated label must end with an ellipsis: {label:?}"
        );
        assert!(
            !label.contains("second line"),
            "only the first line may be shown: {label:?}"
        );
    }

    #[test]
    fn tool_nodes_render_short_name_tag_only() {
        let mut tool = e("1", None, "tool");
        tool.message = Some(Message::ToolResult(pi_ai::ToolResultMessage {
            tool_call_id: "call_1".into(),
            tool_name: "bash".into(),
            content: vec![pi_ai::ContentBlock::text(
                "FOO=bar\nBAZ=qux\nsome long ls output line that would wrap",
            )],
            usage: None,
            details: None,
            added_tool_names: vec![],
            is_error: false,
            timestamp: 0,
        }));
        let panel = TreePanel::new(
            SessionTreeResult {
                tree: vec![n(tool, vec![])],
                leaf_id: None,
                active_leaf_id: None,
            },
            TreePanelMode::Navigate,
        );
        assert_eq!(panel.visible()[0].label_text, "[tool: bash]");
    }

    #[test]
    fn long_single_line_labels_truncate_to_the_cap() {
        let user = e("1", None, &"x".repeat(200));
        let panel = TreePanel::new(
            SessionTreeResult {
                tree: vec![n(user, vec![])],
                leaf_id: None,
                active_leaf_id: None,
            },
            TreePanelMode::Navigate,
        );
        let label = &panel.visible()[0].label_text;
        assert!(
            label.starts_with("user: "),
            "label must keep its type prefix: {label:?}"
        );
        assert!(
            !label.contains('\n'),
            "label must be a single line: {label:?}"
        );
        assert_eq!(label.chars().count(), NODE_LABEL_MAX);
        assert!(label.ends_with('…'));
    }

    #[test]
    fn search_still_matches_content_beyond_the_first_line() {
        let mut p = TreePanel::new(
            SessionTreeResult {
                tree: vec![n(e("1", None, "first line\nneedle deep"), vec![])],
                leaf_id: None,
                active_leaf_id: None,
            },
            TreePanelMode::Navigate,
        );
        assert_eq!(p.visible()[0].label_text, "user: first line");
        p.insert_search_char('n');
        p.insert_search_char('d');
        assert_eq!(
            p.visible().len(),
            1,
            "search must match full content, not just the truncated label"
        );
    }

    #[test]
    fn branching_rows_keep_depth_gutters_and_fold_state() {
        // root
        // ├── a
        // │   ├── a1
        // │   └── a2
        // └── b
        //     └── b1
        let tree = n(
            e("root", None, "r"),
            vec![
                n(
                    e("a", Some("root"), "a"),
                    vec![
                        n(e("a1", Some("a"), "a1"), vec![]),
                        n(e("a2", Some("a"), "a2"), vec![]),
                    ],
                ),
                n(
                    e("b", Some("root"), "b"),
                    vec![n(e("b1", Some("b"), "b1"), vec![])],
                ),
            ],
        );
        let mut p = TreePanel::new(
            SessionTreeResult {
                tree: vec![tree],
                leaf_id: None,
                active_leaf_id: None,
            },
            TreePanelMode::Navigate,
        );
        let ids: Vec<&str> = p.visible().iter().map(|node| node.id.as_str()).collect();
        assert_eq!(ids, ["root", "a", "a1", "a2", "b", "b1"]);
        let depths: Vec<usize> = p.visible().iter().map(|node| node.depth).collect();
        assert_eq!(depths, [0, 1, 2, 2, 1, 2]);
        let is_last: Vec<bool> = p.visible().iter().map(|node| node.is_last).collect();
        assert_eq!(is_last, [true, false, false, true, true, true]);
        let gutters: Vec<Vec<bool>> = p
            .visible()
            .iter()
            .map(|node| node.gutters.into_iter().copied().collect())
            .collect();
        assert_eq!(
            gutters,
            [
                vec![],
                vec![false],
                vec![false, true],
                vec![false, true],
                vec![false],
                vec![false, false]
            ]
        );
        let foldable: Vec<bool> = p.visible().iter().map(|node| node.foldable).collect();
        assert_eq!(foldable, [true, true, false, false, true, false]);

        // Folding "a" hides its subtree and keeps remaining gutters consistent.
        p.apply_action(Action::EditorDown, 10);
        p.apply_action(Action::EditorLeft, 10);
        let ids: Vec<&str> = p.visible().iter().map(|node| node.id.as_str()).collect();
        assert_eq!(ids, ["root", "a", "b", "b1"]);
        assert!(p.visible()[1].folded);
        assert!(p.visible()[1].foldable);
        let gutters: Vec<Vec<bool>> = p
            .visible()
            .iter()
            .map(|node| node.gutters.into_iter().copied().collect())
            .collect();
        assert_eq!(
            gutters,
            [vec![], vec![false], vec![false], vec![false, false]]
        );
    }

    fn linear_tree(depth: usize) -> SessionTreeResult {
        let mut node = n(
            e(
                &format!("n{depth}"),
                Some(&format!("n{}", depth - 1)),
                "deepest",
            ),
            vec![],
        );
        for level in (0..depth).rev() {
            let parent = if level == 0 {
                None
            } else {
                Some(format!("n{level}"))
            };
            node = n(
                e(&format!("n{level}"), parent.as_deref(), "level"),
                vec![node],
            );
        }
        SessionTreeResult {
            tree: vec![node],
            leaf_id: Some(format!("n{depth}")),
            active_leaf_id: Some(format!("n{depth}")),
        }
    }

    #[test]
    fn deep_linear_tree_builds_and_selects_in_both_modes() {
        // A 20,001-entry linear chain must construct and reach its deepest
        // row in linear time and storage; the previous per-row ancestor
        // chains, effective-parent climbs and subtree scans made this O(n²).
        let depth = 20_000;
        let deepest_id = format!("n{depth}");

        let mut navigate = TreePanel::new(linear_tree(depth), TreePanelMode::Navigate);
        assert_eq!(navigate.visible().len(), depth + 1);
        assert_eq!(navigate.selected_id(), Some("n0"));
        let deepest = &navigate.visible()[depth];
        assert_eq!(deepest.id, deepest_id);
        assert_eq!(deepest.depth, depth);
        assert!(deepest.is_last);
        assert!(deepest.active);
        let gutters: Vec<bool> = deepest.gutters.into_iter().copied().collect();
        assert_eq!(gutters.len(), depth);
        assert!(gutters.iter().all(|gutter| !gutter));
        navigate.apply_action(Action::EditorPageDown, depth);
        assert_eq!(navigate.selected, depth);
        assert_eq!(navigate.selected_id(), Some(deepest_id.as_str()));

        let fork = TreePanel::new(linear_tree(depth), TreePanelMode::Fork);
        assert_eq!(fork.visible().len(), depth + 1);
        assert_eq!(fork.selected_id(), Some(deepest_id.as_str()));
        assert!(fork.selected_is_forkable());
    }
}
