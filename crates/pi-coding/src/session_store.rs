//! Native Pi v3 append-only JSONL session storage and tree reconstruction.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, SecondsFormat, Utc};
use parking_lot::Mutex;
use pi_ai::{BranchSummaryMessage, CompactionSummaryMessage, ContentBlock, CustomMessage, CustomMessageContent, Message, Model, Usage};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::resources::agent_dir;
use crate::TodoState;
use crate::import::{
    OpenedSource, open_native_session_for_append_direct,
    open_native_session_for_append_under_root,
};

pub const CURRENT_SESSION_VERSION: u32 = 3;

const MAX_SESSION_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_SESSION_LINE_BYTES: usize = 8 * 1024 * 1024;
const MAX_SESSION_RECORDS: usize = 100_000;
const MAX_RECONSTRUCTED_MESSAGES: usize = 100_000;


#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionHeader {
    #[serde(rename = "type")]
    pub record_type: String,
    pub version: u32,
    pub id: String,
    pub timestamp: String,
    pub cwd: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionRecord {
    Session {
        version: u32,
        id: String,
        timestamp: String,
        cwd: PathBuf,
    },
    Message {
        id: String,
        #[serde(rename = "parentId")]
        parent_id: Option<String>,
        timestamp: String,
        message: Message,
    },
    ModelChange {
        id: String,
        #[serde(rename = "parentId")]
        parent_id: Option<String>,
        timestamp: String,
        provider: String,
        #[serde(rename = "modelId")]
        model_id: String,
    },
    ThinkingLevelChange {
        id: String,
        #[serde(rename = "parentId")]
        parent_id: Option<String>,
        timestamp: String,
        #[serde(rename = "thinkingLevel")]
        thinking_level: String,
    },
    Compaction {
        id: String,
        #[serde(rename = "parentId")]
        parent_id: Option<String>,
        timestamp: String,
        summary: String,
        #[serde(rename = "firstKeptEntryId", skip_serializing_if = "Option::is_none")]
        first_kept_entry_id: Option<String>,
        #[serde(rename = "tokensBefore", default)]
        tokens_before: i64,
        #[serde(rename = "retainedTail", default, skip_serializing_if = "Vec::is_empty")]
        retained_tail: Vec<Message>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        #[serde(rename = "fromHook", default, skip_serializing_if = "Option::is_none")]
        from_hook: Option<bool>,
    },
    SessionInfo {
        id: String,
        #[serde(rename = "parentId")]
        parent_id: Option<String>,
        timestamp: String,
        name: String,
    },
    BranchSummary {
        id: String,
        #[serde(rename = "parentId")]
        parent_id: Option<String>,
        timestamp: String,
        #[serde(rename = "fromId")]
        from_id: String,
        summary: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        #[serde(rename = "fromHook", default, skip_serializing_if = "Option::is_none")]
        from_hook: Option<bool>,
    },
    Custom {
        id: String,
        #[serde(rename = "parentId")]
        parent_id: Option<String>,
        timestamp: String,
        #[serde(rename = "customType")]
        custom_type: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<Value>,
    },
    CustomMessage {
        id: String,
        #[serde(rename = "parentId")]
        parent_id: Option<String>,
        timestamp: String,
        #[serde(rename = "customType")]
        custom_type: String,
        content: CustomMessageContent,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<Value>,
        display: bool,
    },
    TodoSnapshot {
        id: String,
        #[serde(rename = "parentId")]
        parent_id: Option<String>,
        timestamp: String,
        state: TodoState,
    },
    Label {
        id: String,
        #[serde(rename = "parentId")]
        parent_id: Option<String>,
        timestamp: String,
        #[serde(rename = "targetId")]
        target_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
}


#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub path: PathBuf,
    pub id: String,
    pub cwd: PathBuf,
    pub timestamp: String,
    pub messages: usize,
    pub name: Option<String>,
    pub first_message: String,
    pub all_messages_text: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionEntry {
    #[serde(rename = "type")]
    pub entry_type: String,
    pub id: String,
    pub parent_id: Option<String>,
    pub timestamp: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_kept_entry_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_before: Option<i64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub retained_tail: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<CustomMessageContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_hook: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom_type: Option<String>,
    #[serde(rename = "state", skip_serializing_if = "Option::is_none")]
    pub todo_state: Option<TodoState>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionTreeNode {
    pub entry: SessionEntry,
    pub children: Vec<SessionTreeNode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label_timestamp: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ForkMessage {
    pub entry_id: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionEntries {
    pub entries: Vec<SessionEntry>,
    pub leaf_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionTreeResult {
    pub tree: Vec<SessionTreeNode>,
    pub leaf_id: Option<String>,
    pub active_leaf_id: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct BranchContext {
    pub messages: Vec<Message>,
    pub thinking_level: String,
    pub provider: Option<String>,
    pub model_id: Option<String>,
}

#[derive(Debug, Clone)]
struct ResolvedLabel {
    label: String,
    timestamp: String,
}

#[derive(Debug, Clone)]
pub struct SessionTree {
    pub header: SessionHeader,
    pub entries: Vec<SessionEntry>,
    pub leaf_id: Option<String>,
    pub active_leaf_id: Option<String>,
    by_id: HashMap<String, usize>,
    children_by_parent: HashMap<Option<String>, Vec<usize>>,
    labels: HashMap<String, ResolvedLabel>,
}

impl SessionTree {
    #[must_use]
    pub fn branch(&self, leaf_id: Option<&str>) -> Vec<&SessionEntry> {
        let leaf = match leaf_id {
            Some(id) => self.by_id.get(id).copied(),
            None => self
                .active_leaf_id
                .as_deref()
                .and_then(|id| self.by_id.get(id).copied()),
        };
        let Some(mut index) = leaf else {
            return Vec::new();
        };
        let mut branch = Vec::new();
        let mut visited = HashSet::new();
        loop {
            let entry = &self.entries[index];
            if !visited.insert(entry.id.as_str()) {
                break;
            }
            branch.push(entry);
            let Some(parent_id) = entry.parent_id.as_deref() else {
                break;
            };
            let Some(parent) = self.by_id.get(parent_id) else {
                break;
            };
            index = *parent;
        }
        branch.reverse();
        branch
    }

    #[must_use]
    pub fn tree(&self) -> Vec<SessionTreeNode> {
        fn build(
            tree: &SessionTree,
            index: usize,
            visiting: &mut HashSet<usize>,
        ) -> SessionTreeNode {
            let resolved = tree.labels.get(&tree.entries[index].id);
            if !visiting.insert(index) {
                return SessionTreeNode {
                    entry: tree.entries[index].clone(),
                    children: Vec::new(),
                    label: resolved.map(|label| label.label.clone()),
                    label_timestamp: resolved.map(|label| label.timestamp.clone()),
                };
            }
            let nodes = tree
                .children_by_parent
                .get(&Some(tree.entries[index].id.clone()))
                .into_iter()
                .flatten()
                .copied()
                .map(|child| build(tree, child, visiting))
                .collect();
            visiting.remove(&index);
            SessionTreeNode {
                entry: tree.entries[index].clone(),
                children: nodes,
                label: resolved.map(|label| label.label.clone()),
                label_timestamp: resolved.map(|label| label.timestamp.clone()),
            }
        }
        self.children_by_parent
            .get(&None)
            .into_iter()
            .flatten()
            .copied()
            .map(|index| build(self, index, &mut HashSet::new()))
            .collect()
    }

    #[must_use]
    pub fn has_thinking_entry(&self) -> bool {
        self.branch(None)
            .iter()
            .any(|entry| entry.entry_type == "thinking_level_change")
    }

    #[must_use]
    pub fn session_name(&self) -> Option<String> {
        self.entries
            .iter()
            .rev()
            .find(|entry| entry.entry_type == "session_info")
            .and_then(|entry| entry.name.as_deref())
            .and_then(normalize_session_name)
    }
    #[must_use]
    pub fn latest_todo_state(&self) -> Option<TodoState> {
        self.branch(None)
            .into_iter()
            .rev()
            .find_map(|entry| entry.todo_state.clone())
    }


    #[must_use]
    pub fn build_context(&self, leaf_id: Option<&str>) -> BranchContext {
        let branch = self.branch(leaf_id);
        let mut context = BranchContext {
            thinking_level: "off".to_owned(),
            ..BranchContext::default()
        };
        let mut latest_compaction = None;
        for (index, entry) in branch.iter().enumerate() {
            match entry.entry_type.as_str() {
                "thinking_level_change" => {
                    if let Some(level) = &entry.thinking_level {
                        context.thinking_level.clone_from(level);
                    }
                }
                "model_change" => {
                    context.provider.clone_from(&entry.provider);
                    context.model_id.clone_from(&entry.model_id);
                }
                "message" => {
                    if let Some(Message::Assistant(message)) = &entry.message {
                        context.provider = Some(message.provider.clone());
                        context.model_id = Some(message.model.clone());
                    }
                }
                "compaction" => latest_compaction = Some(index),
                _ => {}
            }
        }

        if let Some(compaction_index) = latest_compaction {
            let compaction = branch[compaction_index];
            if let Some(summary) = compaction.summary.as_deref() {
                context.messages.push(Message::CompactionSummary(CompactionSummaryMessage {
                    summary: summary.to_owned(),
                    tokens_before: compaction.tokens_before.unwrap_or_default(),
                    details: compaction.details.clone(),
                    usage: compaction.usage.clone(),
                    from_hook: compaction.from_hook,
                    timestamp: timestamp_millis(&compaction.timestamp),
                }));
            }
            if !compaction.retained_tail.is_empty() {
                context
                    .messages
                    .extend(compaction.retained_tail.iter().cloned());
            } else if let Some(first_kept) = compaction.first_kept_entry_id.as_deref() {
                if let Some(start) = branch[..compaction_index]
                    .iter()
                    .position(|entry| entry.id == first_kept)
                {
                    for entry in &branch[start..compaction_index] {
                        append_entry_message(&mut context.messages, entry);
                    }
                }
            }
            for entry in &branch[compaction_index + 1..] {
                append_entry_message(&mut context.messages, entry);
            }
        } else {
            for entry in branch {
                append_entry_message(&mut context.messages, entry);
            }
        }
        context
    }
}

/// Snapshot used to compare-and-append a durable custom record without allowing
/// branch navigation or another recorder mutation to redirect the append.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionAppendToken {
    active_leaf_id: Option<String>,
    revision: u64,
}

#[derive(Debug)]
struct RecorderState {
    path: PathBuf,
    id: String,
    timestamp: String,
    cwd: PathBuf,
    parent_session: Option<String>,
    last_id: Option<String>,
    active_leaf_id: Option<String>,
    revision: u64,
    used_ids: HashSet<String>,
    pending: Vec<Value>,
    file: Option<File>,
    flushed: bool,
    has_assistant: bool,
    session_name: Option<String>,
    /// When true, every record from the header onward is durable-appended
    /// (write + flush + fsync) so a crash-durable child transcript survives
    /// interruption before the first assistant message.
    durable: bool,
}

/// A fully parsed native session whose read/append descriptor is retained from
/// secure open through recorder construction.
#[derive(Debug)]
pub struct PreparedSessionResume {
    path: PathBuf,
    tree: SessionTree,
    file: File,
    requires_separator: bool,
}

impl PreparedSessionResume {
    /// Prepare an explicitly authorized native session path. The parent
    /// directory is opened as a capability and the final component is not
    /// followed.
    pub fn prepare_path(path: impl AsRef<Path>) -> Result<Self> {
        let opened = open_native_session_for_append_direct(path.as_ref())?;
        Self::from_opened(opened)
    }

    /// Prepare a native session confined beneath a configured catalog root.
    pub(crate) fn prepare_under_root(root: &Path, path: &Path) -> Result<Self> {
        let opened = open_native_session_for_append_under_root(root, path)?;
        Self::from_opened(opened)
    }

    fn from_opened(opened: OpenedSource) -> Result<Self> {
        let path = opened.path().to_path_buf();
        let mut file = opened.into_primary();
        let tree = load_session_tree_from_file(
            file.try_clone()
                .with_context(|| format!("cloning retained session handle {}", path.display()))?,
            &path,
        )?;
        let requires_separator = session_requires_separator(&mut file, &path)?;
        Ok(Self {
            path,
            tree,
            file,
            requires_separator,
        })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn tree(&self) -> &SessionTree {
        &self.tree
    }

    #[must_use]
    pub fn target_cwd(&self) -> &Path {
        &self.tree.header.cwd
    }

    #[must_use]
    pub fn build_context(&self) -> BranchContext {
        self.tree.build_context(None)
    }

    /// Consume the prepared session and transfer its retained append handle to
    /// a recorder. No pathname is reopened.
    pub fn into_recorder(mut self) -> Result<SessionRecorder> {
        if self.requires_separator {
            self.file.write_all(b"\n").with_context(|| {
                format!("separating final session record in {}", self.path.display())
            })?;
            self.file.flush().with_context(|| {
                format!("flushing session separator in {}", self.path.display())
            })?;
        }
        let session_name = self.tree.session_name();
        let used_ids = self
            .tree
            .entries
            .iter()
            .map(|entry| entry.id.clone())
            .collect();
        let active_leaf_id = self.tree.leaf_id.clone();
        Ok(SessionRecorder {
            inner: Arc::new(Mutex::new(RecorderState {
                path: self.path,
                id: self.tree.header.id,
                timestamp: self.tree.header.timestamp,
                cwd: self.tree.header.cwd,
                parent_session: self.tree.header.parent_session,
                last_id: self.tree.leaf_id,
                active_leaf_id,
                revision: 0,
                used_ids,
                pending: Vec::new(),
                file: Some(self.file),
                flushed: true,
                has_assistant: true,
                session_name,
                durable: false,
            })),
        })
    }
}

#[derive(Debug, Clone)]
pub struct SessionRecorder {
    inner: Arc<Mutex<RecorderState>>,
}

impl SessionRecorder {
    #[must_use]
    pub fn path(&self) -> PathBuf {
        self.inner.lock().path.clone()
    }

    #[must_use]
    pub fn id(&self) -> String {
        self.inner.lock().id.clone()
    }

    #[must_use]
    pub fn header(&self) -> SessionHeader {
        let state = self.inner.lock();
        SessionHeader {
            record_type: "session".to_owned(),
            version: CURRENT_SESSION_VERSION,
            id: state.id.clone(),
            timestamp: state.timestamp.clone(),
            cwd: state.cwd.clone(),
            parent_session: state.parent_session.clone(),
        }
    }

    #[must_use]
    pub fn last_entry_id(&self) -> Option<String> {
        self.inner.lock().last_id.clone()
    }

    #[must_use]
    pub fn active_leaf_id(&self) -> Option<String> {
        self.inner.lock().active_leaf_id.clone()
    }
    /// Captures the active leaf and recorder revision for a later atomic append.
    #[must_use]
    pub fn append_token(&self) -> SessionAppendToken {
        let state = self.inner.lock();
        append_token_from_state(&state)
    }


    pub fn branch(&self, entry_id: &str) -> Result<()> {
        let mut state = self.inner.lock();
        if !state.used_ids.contains(entry_id) {
            bail!("Entry not found: {entry_id}");
        }
        if state.active_leaf_id.as_deref() != Some(entry_id) {
            state.active_leaf_id = Some(entry_id.to_owned());
            state.revision = state.revision.saturating_add(1);
        }
        Ok(())
    }

    pub fn reset_leaf(&self) {
        let mut state = self.inner.lock();
        if state.active_leaf_id.take().is_some() {
            state.revision = state.revision.saturating_add(1);
        }
    }

    pub fn branch_with_summary(
        &self,
        branch_from_id: Option<&str>,
        summary: &str,
    ) -> Result<String> {
        self.branch_with_summary_metadata(branch_from_id, summary, None, None, None)
    }

    pub fn branch_with_summary_metadata(
        &self,
        branch_from_id: Option<&str>,
        summary: &str,
        details: Option<&Value>,
        usage: Option<&Usage>,
        from_hook: Option<bool>,
    ) -> Result<String> {
        let mut state = self.inner.lock();
        if let Some(entry_id) = branch_from_id
            && !state.used_ids.contains(entry_id)
        {
            bail!("Entry not found: {entry_id}");
        }
        let previous_active_leaf_id = state.active_leaf_id.clone();
        let previous_revision = state.revision;
        let target_leaf = branch_from_id.map(str::to_owned);
        if state.active_leaf_id != target_leaf {
            state.active_leaf_id = target_leaf;
            state.revision = state.revision.saturating_add(1);
        }
        let mut fields = Map::new();
        fields.insert(
            "fromId".to_owned(),
            Value::String(branch_from_id.unwrap_or("root").to_owned()),
        );
        fields.insert("summary".to_owned(), Value::String(summary.to_owned()));
        if let Some(details) = details {
            fields.insert("details".to_owned(), details.clone());
        }
        if let Some(usage) = usage {
            fields.insert("usage".to_owned(), serde_json::to_value(usage)?);
        }
        if let Some(from_hook) = from_hook {
            fields.insert("fromHook".to_owned(), Value::Bool(from_hook));
        }
        let result = append_entry(&mut state, "branch_summary", Value::Object(fields));
        if result.is_err() {
            state.active_leaf_id = previous_active_leaf_id;
            state.revision = previous_revision;
        }
        result
    }

    pub fn record_label(&self, target_id: &str, label: Option<&str>) -> Result<String> {
        let mut state = self.inner.lock();
        if !state.used_ids.contains(target_id) {
            bail!("Entry not found: {target_id}");
        }
        let previous_last_id = state.last_id.clone();
        let previous_active_leaf_id = state.active_leaf_id.clone();
        let mut fields = Map::new();
        fields.insert("targetId".to_owned(), json!(target_id));
        if let Some(label) = label {
            fields.insert("label".to_owned(), json!(label));
        }
        let result = append_entry(&mut state, "label", Value::Object(fields));
        if result.is_ok() {
            state.last_id = previous_last_id;
            state.active_leaf_id = previous_active_leaf_id;
        }
        result
    }

    pub fn fork_from(&self, entry_id: Option<&str>) {
        let mut state = self.inner.lock();
        let entry_id = entry_id.map(str::to_owned);
        if state.active_leaf_id != entry_id || state.last_id != entry_id {
            state.last_id.clone_from(&entry_id);
            state.active_leaf_id = entry_id;
            state.revision = state.revision.saturating_add(1);
        }
    }

    pub fn record_message(&self, message: &Message) -> Result<String> {
        let mut state = self.inner.lock();
        let previous_has_assistant = state.has_assistant;
        if matches!(message, Message::Assistant(_)) {
            state.has_assistant = true;
        }
        match append_entry(&mut state, "message", json!({ "message": message })) {
            Ok(id) => Ok(id),
            Err(error) => {
                state.has_assistant = previous_has_assistant;
                Err(error)
            }
        }
    }

    pub fn record_custom_entry(
        &self,
        custom_type: &str,
        data: Option<Value>,
    ) -> Result<String> {
        let mut fields = Map::new();
        fields.insert("customType".to_owned(), Value::String(custom_type.to_owned()));
        if let Some(data) = data {
            fields.insert("data".to_owned(), data);
        }
        let mut state = self.inner.lock();
        append_entry(&mut state, "custom", Value::Object(fields))
    }

    /// Compare-and-appends a typed custom record after validating the live tree.
    ///
    /// Token check, tree capture, validation, append, flush, and sync all happen
    /// under the recorder mutex so branch/reset cannot redirect the append.
    pub fn record_custom_entry_durable_checked<T, F>(
        &self,
        expected: &SessionAppendToken,
        custom_type: &str,
        data: &T,
        validate: F,
    ) -> Result<String>
    where
        T: serde::Serialize,
        F: FnOnce(&SessionTree) -> Result<()>,
    {
        let mut state = self.inner.lock();
        if state.active_leaf_id != expected.active_leaf_id || state.revision != expected.revision {
            bail!("session changed before durable custom append");
        }
        let tree = session_tree_from_state(&state)?;
        validate(&tree)?;
        let mut fields = Map::new();
        fields.insert("customType".to_owned(), Value::String(custom_type.to_owned()));
        fields.insert("data".to_owned(), serde_json::to_value(data)?);
        append_entry_durable(&mut state, "custom", Value::Object(fields))
    }


    pub fn record_custom_message(&self, message: &CustomMessage) -> Result<String> {
        let mut fields = serde_json::to_value(message)?;
        let Value::Object(ref mut object) = fields else {
            unreachable!("custom messages serialize to JSON objects");
        };
        object.remove("role");
        object.remove("timestamp");
        let mut state = self.inner.lock();
        append_entry(&mut state, "custom_message", fields)
    }

    pub fn record_todo_snapshot(&self, state: &TodoState) -> Result<String> {
        let mut recorder = self.inner.lock();
        append_entry(&mut recorder, "todo_snapshot", json!({ "state": state }))
    }

    pub fn latest_todo_state(&self) -> Result<Option<TodoState>> {
        Ok(self.tree()?.latest_todo_state())
    }

    pub fn record_model_change(&self, provider: &str, model_id: &str) -> Result<String> {
        let mut state = self.inner.lock();
        append_entry(
            &mut state,
            "model_change",
            json!({ "provider": provider, "modelId": model_id }),
        )
    }

    pub fn record_thinking_level(&self, thinking_level: &str) -> Result<String> {
        let mut state = self.inner.lock();
        append_entry(
            &mut state,
            "thinking_level_change",
            json!({ "thinkingLevel": thinking_level }),
        )
    }

    pub fn record_compaction(
        &self,
        summary: &str,
        first_kept_entry_id: Option<&str>,
        tokens_before: i64,
        retained_tail: &[Message],
    ) -> Result<String> {
        self.record_compaction_metadata(
            summary,
            first_kept_entry_id,
            tokens_before,
            retained_tail,
            None,
            None,
            None,
        )
    }

    pub fn record_compaction_metadata(
        &self,
        summary: &str,
        first_kept_entry_id: Option<&str>,
        tokens_before: i64,
        retained_tail: &[Message],
        details: Option<&Value>,
        usage: Option<&Usage>,
        from_hook: Option<bool>,
    ) -> Result<String> {
        let mut state = self.inner.lock();
        let mut fields = Map::new();
        fields.insert("summary".to_owned(), Value::String(summary.to_owned()));
        if let Some(first_kept_entry_id) = first_kept_entry_id {
            fields.insert(
                "firstKeptEntryId".to_owned(),
                Value::String(first_kept_entry_id.to_owned()),
            );
        }
        fields.insert("tokensBefore".to_owned(), json!(tokens_before));
        fields.insert("retainedTail".to_owned(), serde_json::to_value(retained_tail)?);
        if let Some(details) = details {
            fields.insert("details".to_owned(), details.clone());
        }
        if let Some(usage) = usage {
            fields.insert("usage".to_owned(), serde_json::to_value(usage)?);
        }
        if let Some(from_hook) = from_hook {
            fields.insert("fromHook".to_owned(), Value::Bool(from_hook));
        }
        append_entry(&mut state, "compaction", Value::Object(fields))
    }

    pub fn record_session_name(&self, name: &str) -> Result<Option<String>> {
        let normalized = normalize_session_name(name);
        let mut state = self.inner.lock();
        append_entry(
            &mut state,
            "session_info",
            json!({ "name": normalized.as_deref().unwrap_or_default() }),
        )?;
        state.session_name.clone_from(&normalized);
        Ok(normalized)
    }

    #[must_use]
    pub fn session_name(&self) -> Option<String> {
        self.inner.lock().session_name.clone()
    }

    pub fn tree(&self) -> Result<SessionTree> {
        session_tree_from_state(&self.inner.lock())
    }

    /// Returns a consistent tree and append token captured under one recorder lock.
    pub fn tree_with_append_token(&self) -> Result<(SessionTree, SessionAppendToken)> {
        let state = self.inner.lock();
        let tree = session_tree_from_state(&state)?;
        Ok((tree, append_token_from_state(&state)))
    }

    pub fn persist_now(&self) -> Result<()> {
        let mut state = self.inner.lock();
        let previous_has_assistant = state.has_assistant;
        state.has_assistant = true;
        if let Err(error) = persist(&mut state) {
            state.has_assistant = previous_has_assistant;
            return Err(error);
        }
        state.has_assistant = previous_has_assistant;
        Ok(())
    }

    /// Like [`persist_now`] but forces a durable append (write + flush + fsync).
    pub fn persist_now_durable(&self) -> Result<()> {
        let mut state = self.inner.lock();
        let previous_has_assistant = state.has_assistant;
        state.has_assistant = true;
        if let Err(error) = persist_durable(&mut state) {
            state.has_assistant = previous_has_assistant;
            return Err(error);
        }
        state.has_assistant = previous_has_assistant;
        Ok(())
    }

    /// Mark this recorder as durable so all subsequent appends use
    /// write+flush+fsync from the header onward.
    pub fn set_durable(&self) {
        self.inner.lock().durable = true;
    }

    pub fn close(&self) -> Result<()> {
        let mut state = self.inner.lock();
        persist_durable(&mut state)?;
        if let Some(file) = state.file.as_mut() {
            file.flush().context("flushing session file")?;
            file.sync_all().context("syncing session file")?;
        }
        state.file = None;
        Ok(())
    }
}

pub fn default_session_dir(cwd: impl AsRef<Path>) -> PathBuf {
    let cwd = absolute_path(cwd.as_ref());
    PathBuf::from(agent_dir())
        .join("sessions")
        .join(format!("--{}--", encode_cwd_safe_path(&cwd)))
}

fn new_session_path(directory: &Path, timestamp: &str, id: &str, has_explicit_id: bool) -> PathBuf {
    if has_explicit_id {
        return directory.join(format!("{id}.jsonl"));
    }
    directory.join(format!("{}_{}.jsonl", timestamp.replace([':', '.'], "-"), id))
}

pub fn start_session(
    cwd: impl AsRef<Path>,
    model: Option<&Model>,
    thinking_level: Option<&str>,
) -> Result<SessionRecorder> {
    start_session_in(cwd, model, thinking_level, None, None, None)
}

pub fn start_session_with_parent(
    cwd: impl AsRef<Path>,
    model: Option<&Model>,
    thinking_level: Option<&str>,
    parent_session: Option<&Path>,
) -> Result<SessionRecorder> {
    start_session_in(cwd, model, thinking_level, None, None, parent_session)
}

pub fn start_session_in(
    cwd: impl AsRef<Path>,
    model: Option<&Model>,
    thinking_level: Option<&str>,
    session_dir: Option<&Path>,
    session_id: Option<&str>,
    parent_session: Option<&Path>,
) -> Result<SessionRecorder> {
    let cwd = absolute_path(cwd.as_ref());
    let directory = session_dir.map_or_else(|| default_session_dir(&cwd), absolute_path);
    fs::create_dir_all(&directory)
        .with_context(|| format!("creating session directory {}", directory.display()))?;
    let has_explicit_id = session_id.is_some();
    let id = match session_id {
        Some(id) => { validate_session_id(id)?; id.to_owned() }
        None => Uuid::now_v7().to_string(),
    };
    if list_sessions_in(&cwd, Some(&directory)).iter().any(|session| session.id == id) {
        bail!("Session already exists with id '{id}'");
    }
    let timestamp = iso_now();
    let path = new_session_path(&directory, &timestamp, &id, has_explicit_id);
    let parent_session = parent_session.map(|path| path.to_string_lossy().into_owned());
    let header = json!({
        "type": "session", "version": CURRENT_SESSION_VERSION, "id": id,
        "timestamp": timestamp, "cwd": cwd, "parentSession": parent_session,
    });
    let recorder = SessionRecorder { inner: Arc::new(Mutex::new(RecorderState {
        path, id, timestamp, cwd, parent_session,
        last_id: None, active_leaf_id: None, revision: 0, used_ids: HashSet::new(),
        pending: vec![header], file: None, flushed: false,
        has_assistant: false, session_name: None, durable: false,
    })) };
    if has_explicit_id {
        recorder.persist_now().context("reserving explicit session id")?;
    }
    if let Some(model) = model { recorder.record_model_change(&model.provider, &model.id)?; }
    if let Some(level) = thinking_level.filter(|level| !level.is_empty()) {
        recorder.record_thinking_level(level)?;
    }
    Ok(recorder)
}

pub fn create_branched_session(source_path: impl AsRef<Path>, leaf_id: &str) -> Result<SessionRecorder> {
    create_branched_session_in(source_path, leaf_id, None)
}

pub fn create_branched_session_in(
    source_path: impl AsRef<Path>,
    leaf_id: &str,
    session_dir: Option<&Path>,
) -> Result<SessionRecorder> {
    let source_path = source_path.as_ref();
    let tree = load_session_tree(source_path)?;
    if !tree.entries.iter().any(|entry| entry.id == leaf_id) {
        bail!("Entry not found: {leaf_id}");
    }

    let mut retained_entries = Vec::new();
    let mut retained_ids = HashSet::new();
    let mut parent_id = None;
    for entry in tree.branch(Some(leaf_id)) {
        if entry.entry_type == "label" {
            continue;
        }
        let mut retained = entry.clone();
        retained.parent_id.clone_from(&parent_id);
        parent_id = Some(retained.id.clone());
        retained_ids.insert(retained.id.clone());
        retained_entries.push(retained);
    }

    let directory = session_dir.map_or_else(
        || {
            source_path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| default_session_dir(&tree.header.cwd))
        },
        absolute_path,
    );
    fs::create_dir_all(&directory)
        .with_context(|| format!("creating session directory {}", directory.display()))?;
    let id = Uuid::now_v7().to_string();
    let timestamp = iso_now();
    let path = directory.join(format!(
        "{}_{}.jsonl",
        timestamp.replace([':', '.'], "-"),
        id
    ));
    let parent_session = Some(source_path.to_string_lossy().into_owned());
    let mut pending = vec![json!({
        "type": "session",
        "version": CURRENT_SESSION_VERSION,
        "id": id,
        "timestamp": timestamp,
        "cwd": tree.header.cwd,
        "parentSession": parent_session,
    })];
    pending.extend(
        retained_entries
            .iter()
            .map(serde_json::to_value)
            .collect::<std::result::Result<Vec<_>, _>>()?,
    );

    let mut allocation_ids = tree
        .entries
        .iter()
        .map(|entry| entry.id.clone())
        .collect::<HashSet<_>>();
    let mut used_ids = retained_ids.clone();
    let mut label_parent_id = parent_id.clone();
    let mut resolved_labels = tree
        .labels
        .iter()
        .filter(|(target_id, _)| retained_ids.contains(*target_id))
        .collect::<Vec<_>>();
    resolved_labels.sort_by(|(left_target, left), (right_target, right)| {
        left.timestamp
            .cmp(&right.timestamp)
            .then_with(|| left_target.cmp(right_target))
    });
    for (target_id, resolved) in resolved_labels {
        let label_id = unique_entry_id(&allocation_ids);
        allocation_ids.insert(label_id.clone());
        used_ids.insert(label_id.clone());
        pending.push(json!({
            "type": "label",
            "id": label_id,
            "parentId": label_parent_id,
            "timestamp": resolved.timestamp,
            "targetId": target_id,
            "label": resolved.label,
        }));
        label_parent_id = Some(label_id);
    }

    let has_assistant = retained_entries
        .iter()
        .any(|entry| matches!(entry.message, Some(Message::Assistant(_))));
    let session_name = retained_entries
        .iter()
        .rev()
        .find(|entry| entry.entry_type == "session_info")
        .and_then(|entry| entry.name.clone());
    let recorder = SessionRecorder {
        inner: Arc::new(Mutex::new(RecorderState {
            path,
            id,
            timestamp,
            cwd: tree.header.cwd,
            parent_session,
            last_id: parent_id.clone(),
            active_leaf_id: parent_id,
            revision: 0,
            used_ids,
            pending,
            file: None,
            flushed: false,
            has_assistant,
            session_name,
            durable: false,
        })),
    };
    if has_assistant {
        persist(&mut recorder.inner.lock())?;
    }
    Ok(recorder)
}

#[cfg(test)]
mod session_directory_compat_tests {
    use super::*;

    #[test]
    fn create_branched_session_in_uses_selected_directory_and_keeps_parent() {
        let cwd = tempfile::tempdir().expect("cwd");
        let source_dir = tempfile::tempdir().expect("source directory");
        let selected_dir = tempfile::tempdir().expect("selected directory");
        let source = start_session_in(
            cwd.path(),
            None,
            Some("off"),
            Some(source_dir.path()),
            Some("source"),
            None,
        )
        .expect("source session");
        let first = source
            .record_message(&Message::user_text("first", 0))
            .expect("first message");
        source
            .record_message(&Message::user_text("second", 1))
            .expect("second message");
        source.persist_now().expect("persist source");
        let source_path = source.path();

        let branch = create_branched_session_in(&source_path, &first, Some(selected_dir.path()))
            .expect("branch session");
        assert_eq!(branch.path().parent(), Some(selected_dir.path()));
        assert_eq!(
            branch.tree().expect("branch tree").header.parent_session.as_deref(),
            Some(source_path.to_string_lossy().as_ref())
        );
    }
}

pub fn fork_session_in(
    source_path: impl AsRef<Path>,
    target_cwd: impl AsRef<Path>,
    session_dir: Option<&Path>,
    session_id: Option<&str>,
) -> Result<SessionRecorder> {
    let source_path = source_path.as_ref();
    let tree = load_session_tree(source_path)?;
    let target_cwd = absolute_path(target_cwd.as_ref());
    let directory = session_dir.map_or_else(|| default_session_dir(&target_cwd), absolute_path);
    fs::create_dir_all(&directory)
        .with_context(|| format!("creating session directory {}", directory.display()))?;
    let has_explicit_id = session_id.is_some();
    let id = match session_id {
        Some(id) => { validate_session_id(id)?; id.to_owned() }
        None => Uuid::now_v7().to_string(),
    };
    if list_sessions_in(&target_cwd, Some(&directory)).iter().any(|session| session.id == id) {
        bail!("Session already exists with id '{id}'");
    }
    let mut retained_entries = Vec::new();
    let mut retained_ids = HashSet::new();
    let mut leaf_id = None;
    for entry in tree.branch(tree.leaf_id.as_deref()) {
        if entry.entry_type == "label" {
            continue;
        }
        let mut retained = entry.clone();
        retained.parent_id.clone_from(&leaf_id);
        leaf_id = Some(retained.id.clone());
        retained_ids.insert(retained.id.clone());
        retained_entries.push(retained);
    }
    let timestamp = iso_now();
    let path = new_session_path(&directory, &timestamp, &id, has_explicit_id);
    let parent_session = Some(source_path.to_string_lossy().into_owned());
    let mut pending = vec![json!({
        "type": "session", "version": CURRENT_SESSION_VERSION, "id": id,
        "timestamp": timestamp, "cwd": target_cwd, "parentSession": parent_session,
    })];
    pending.extend(retained_entries.iter().map(serde_json::to_value)
        .collect::<std::result::Result<Vec<_>, _>>()?);

    let mut allocation_ids = tree.entries.iter().map(|entry| entry.id.clone()).collect::<HashSet<_>>();
    let mut used_ids = retained_ids.clone();
    let mut label_parent_id = leaf_id.clone();
    let mut resolved_labels = tree.labels.iter()
        .filter(|(target_id, _)| retained_ids.contains(*target_id))
        .collect::<Vec<_>>();
    resolved_labels.sort_by(|(left_target, left), (right_target, right)| {
        left.timestamp.cmp(&right.timestamp).then_with(|| left_target.cmp(right_target))
    });
    for (target_id, resolved) in resolved_labels {
        let label_id = unique_entry_id(&allocation_ids);
        allocation_ids.insert(label_id.clone());
        used_ids.insert(label_id.clone());
        pending.push(json!({
            "type": "label", "id": label_id, "parentId": label_parent_id,
            "timestamp": resolved.timestamp, "targetId": target_id, "label": resolved.label,
        }));
        label_parent_id = Some(label_id);
    }
    let has_assistant = retained_entries.iter().any(|entry| matches!(entry.message, Some(Message::Assistant(_))));
    let session_name = retained_entries.iter().rev().find(|entry| entry.entry_type == "session_info")
        .and_then(|entry| entry.name.clone());
    let recorder = SessionRecorder { inner: Arc::new(Mutex::new(RecorderState {
        path, id, timestamp, cwd: target_cwd, parent_session,
        last_id: leaf_id.clone(), active_leaf_id: leaf_id, revision: 0, used_ids, pending,
        file: None, flushed: false, has_assistant, session_name, durable: false,
    })) };
    if has_explicit_id {
        recorder.persist_now().context("reserving explicit fork session id")?;
    } else if has_assistant {
        persist(&mut recorder.inner.lock())?;
    }
    Ok(recorder)
}

pub fn resume_session(path: impl AsRef<Path>) -> Result<SessionRecorder> {
    PreparedSessionResume::prepare_path(path)?.into_recorder()
}

/// Start a durable child session: the header and every subsequent record is
/// durable-appended (write + flush + fsync) so a crash mid-turn leaves a
/// recoverable partial transcript. `session_dir` is the child root (e.g.
/// `<resolved-session-root>/children/<parent-id>/`). `parent_session` is the
/// canonical parent JSONL path.
pub fn start_durable_child_session_in(
    cwd: impl AsRef<Path>,
    model: Option<&Model>,
    thinking_level: Option<&str>,
    session_dir: &Path,
    session_id: Option<&str>,
    parent_session: &Path,
) -> Result<SessionRecorder> {
    let cwd = absolute_path(cwd.as_ref());
    let directory = absolute_path(session_dir);
    fs::create_dir_all(&directory)
        .with_context(|| format!("creating durable child session directory {}", directory.display()))?;
    let has_explicit_id = session_id.is_some();
    let id = match session_id {
        Some(id) => { validate_session_id(id)?; id.to_owned() }
        None => Uuid::now_v7().to_string(),
    };
    let timestamp = iso_now();
    let path = new_session_path(&directory, &timestamp, &id, has_explicit_id);
    let parent_session_str = parent_session.to_string_lossy().into_owned();
    let header = json!({
        "type": "session", "version": CURRENT_SESSION_VERSION, "id": id,
        "timestamp": timestamp, "cwd": cwd, "parentSession": parent_session_str,
    });
    let recorder = SessionRecorder { inner: Arc::new(Mutex::new(RecorderState {
        path, id, timestamp, cwd, parent_session: Some(parent_session_str),
        last_id: None, active_leaf_id: None, revision: 0, used_ids: HashSet::new(),
        pending: vec![header], file: None, flushed: false,
        has_assistant: false, session_name: None, durable: true,
    })) };
    // Durable-append the header immediately so the file exists on disk.
 recorder.persist_now_durable().context("reserving durable child session header")?;
    if let Some(model) = model { recorder.record_model_change(&model.provider, &model.id)?; }
    if let Some(level) = thinking_level.filter(|level| !level.is_empty()) {
        recorder.record_thinking_level(level)?;
    }
    Ok(recorder)
}

/// Resume a durable child session from an existing JSONL path, continuing with
/// durable (fsync) appends. The path must be a regular file directly inside the
/// child root.
pub fn resume_durable_child_session(path: impl AsRef<Path>) -> Result<SessionRecorder> {
    let prepared = PreparedSessionResume::prepare_path(path)?;
    let mut recorder = prepared.into_recorder()?;
    recorder.inner.lock().durable = true;
    Ok(recorder)
}

/// Resume a durable child session from a prepared resume handle, continuing
/// with durable (fsync) appends. Avoids re-opening the file when the caller
/// already prepared it for cwd validation.
pub fn resume_durable_child_session_from_prepared(
    prepared: PreparedSessionResume,
) -> Result<SessionRecorder> {
    let mut recorder = prepared.into_recorder()?;
    recorder.inner.lock().durable = true;
    Ok(recorder)
}

pub fn load_session_tree(path: impl AsRef<Path>) -> Result<SessionTree> {
    let path = path.as_ref();
    let file = File::open(path).with_context(|| format!("opening session {}", path.display()))?;
    load_session_tree_from_file(file, path)
}

fn load_session_tree_from_file(mut file: File, path: &Path) -> Result<SessionTree> {
    file.seek(SeekFrom::Start(0))
        .with_context(|| format!("seeking session {} for parse", path.display()))?;
    let file_size = file
        .metadata()
        .with_context(|| format!("reading session metadata {}", path.display()))?
        .len();
    if file_size > MAX_SESSION_FILE_BYTES {
        bail!(
            "session {} exceeds the 64 MiB file safety limit",
            path.display()
        );
    }

    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    let mut values = Vec::new();
    let mut line_number = 0usize;
    let mut bytes_read = 0u64;
    while let Some(consumed) = read_bounded_session_line(&mut reader, &mut line).with_context(|| {
        format!("reading session {} line {}", path.display(), line_number + 1)
    })? {
        line_number += 1;
        bytes_read = bytes_read
            .checked_add(u64::try_from(consumed).unwrap_or(u64::MAX))
            .ok_or_else(|| anyhow!("session {} byte count overflow", path.display()))?;
        if bytes_read > MAX_SESSION_FILE_BYTES {
            bail!(
                "session {} exceeds the 64 MiB file safety limit while reading line {}",
                path.display(),
                line_number
            );
        }
        let trimmed = trim_ascii_whitespace(&line);
        if trimmed.is_empty() {
            continue;
        }
        if values.len() >= MAX_SESSION_RECORDS {
            bail!(
                "session {} exceeds the 100000 record safety limit at line {}",
                path.display(),
                line_number
            );
        }
        let value = serde_json::from_slice::<Value>(trimmed).with_context(|| {
            format!("parsing session {} line {}", path.display(), line_number)
        })?;
        if !value.is_object() {
            bail!(
                "session {} line {} must be a JSON object",
                path.display(),
                line_number
            );
        }
        values.push(value);
    }
    session_tree_from_values(path, &values)
}

fn session_requires_separator(file: &mut File, path: &Path) -> Result<bool> {
    if file
        .metadata()
        .with_context(|| format!("reading session metadata {}", path.display()))?
        .len()
        == 0
    {
        return Ok(false);
    }
    file.seek(SeekFrom::End(-1))
        .with_context(|| format!("seeking final session byte {}", path.display()))?;
    let mut last_byte = [0_u8; 1];
    file.read_exact(&mut last_byte)
        .with_context(|| format!("reading final session byte {}", path.display()))?;
    Ok(last_byte[0] != b'\n')
}

fn read_bounded_session_line<R: BufRead>(
    reader: &mut R,
    line: &mut Vec<u8>,
) -> io::Result<Option<usize>> {
    line.clear();
    let mut consumed = 0usize;
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok((consumed != 0).then_some(consumed));
        }
        if let Some(newline) = available.iter().position(|byte| *byte == b'\n') {
            if line.len().saturating_add(newline) > MAX_SESSION_LINE_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "session line exceeds the 8 MiB safety limit",
                ));
            }
            line.extend_from_slice(&available[..newline]);
            reader.consume(newline + 1);
            consumed = consumed.saturating_add(newline + 1);
            return Ok(Some(consumed));
        }
        if line.len().saturating_add(available.len()) > MAX_SESSION_LINE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "session line exceeds the 8 MiB safety limit",
            ));
        }
        let available_len = available.len();
        line.extend_from_slice(available);
        reader.consume(available_len);
        consumed = consumed.saturating_add(available_len);
    }
}

fn trim_ascii_whitespace(mut bytes: &[u8]) -> &[u8] {
    while bytes.first().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[1..];
    }
    while bytes.last().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

fn session_tree_from_values(path: &Path, values: &[Value]) -> Result<SessionTree> {
    if values.len() > MAX_SESSION_RECORDS {
        bail!(
            "session {} exceeds the 100000 record safety limit at record {}",
            path.display(),
            MAX_SESSION_RECORDS + 1
        );
    }
    let header_value = values
        .first()
        .and_then(Value::as_object)
        .filter(|value| value.get("type").and_then(Value::as_str) == Some("session"))
        .ok_or_else(|| anyhow!("not a Pi session file (missing session header): {}", path.display()))?;
    let id = nonempty_string(header_value, "id")
        .ok_or_else(|| anyhow!("not a Pi session file (missing session id): {}", path.display()))?;
    let header = SessionHeader {
        record_type: "session".to_owned(),
        version: header_value
            .get("version")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .unwrap_or(1),
        id,
        timestamp: nonempty_string(header_value, "timestamp").unwrap_or_default(),
        cwd: header_value
            .get("cwd")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .unwrap_or_default(),
        parent_session: nonempty_string(header_value, "parentSession"),
    };

    let mut entries = Vec::new();
    let mut by_id = HashMap::new();
    let mut reconstructed_messages = 0usize;
    for value in values.iter().skip(1) {
        let Some(object) = value.as_object() else {
            continue;
        };
        let Some(id) = nonempty_string(object, "id") else {
            continue;
        };
        let entry_type = nonempty_string(object, "type").unwrap_or_default();
        let message = object
            .get("message")
            .and_then(|message| serde_json::from_value::<Message>(message.clone()).ok());
        let retained_tail: Vec<Message> = object
            .get("retainedTail")
            .and_then(Value::as_array)
            .map(|messages| {
                messages
                    .iter()
                    .filter_map(|message| serde_json::from_value::<Message>(message.clone()).ok())
                    .collect()
            })
            .unwrap_or_default();
        let decoded_messages = reconstructed_message_count(
            &entry_type,
            message.as_ref(),
            object,
            &retained_tail,
        );
        reconstructed_messages = reconstructed_messages
            .checked_add(decoded_messages)
            .ok_or_else(|| anyhow!("session {} reconstructed message count overflow", path.display()))?;
        if reconstructed_messages > MAX_RECONSTRUCTED_MESSAGES {
            bail!(
                "session {} exceeds the 100000 reconstructed message safety limit at record {}",
                path.display(),
                entries.len() + 2
            );
        }
        let entry = SessionEntry {
            id: id.clone(),
            parent_id: nonempty_string(object, "parentId"),
            entry_type,
            timestamp: nonempty_string(object, "timestamp").unwrap_or_default(),
            message,
            provider: nonempty_string(object, "provider"),
            model_id: nonempty_string(object, "modelId").or_else(|| {
                nonempty_string(object, "model").and_then(|model| {
                    model
                        .split_once('/')
                        .map_or(Some(model.clone()), |(_, id)| Some(id.to_owned()))
                })
            }),
            thinking_level: nonempty_string(object, "thinkingLevel"),
            summary: nonempty_string(object, "summary"),
            first_kept_entry_id: nonempty_string(object, "firstKeptEntryId"),
            tokens_before: object.get("tokensBefore").and_then(Value::as_i64),
            retained_tail,
            content: object
                .get("content")
                .cloned()
                .and_then(|content| serde_json::from_value(content).ok()),
            display: object.get("display").and_then(Value::as_bool),
            details: object.get("details").cloned(),
            usage: object
                .get("usage")
                .cloned()
                .and_then(|usage| serde_json::from_value(usage).ok()),
            from_hook: object.get("fromHook").and_then(Value::as_bool),
            data: object.get("data").cloned(),
            name: object.get("name").and_then(Value::as_str).map(str::to_owned),
            label: nonempty_string(object, "label"),
            target_id: nonempty_string(object, "targetId"),
            from_id: nonempty_string(object, "fromId"),
            custom_type: nonempty_string(object, "customType"),
            todo_state: object
                .get("state")
                .cloned()
                .and_then(|state| serde_json::from_value(state).ok()),
        };
        by_id.insert(id, entries.len());
        entries.push(entry);
    }
    let leaf_id = entries
        .iter()
        .rev()
        .find(|entry| entry.entry_type != "label")
        .map(|entry| entry.id.clone());
    let (children_by_parent, labels) = build_tree_indexes(&entries, &by_id);
    Ok(SessionTree {
        header,
        entries,
        active_leaf_id: leaf_id.clone(),
        leaf_id,
        by_id,
        children_by_parent,
        labels,
    })
}
fn reconstructed_message_count(
    entry_type: &str,
    message: Option<&Message>,
    object: &Map<String, Value>,
    retained_tail: &[Message],
) -> usize {
    match entry_type {
        "message" => usize::from(message.is_some()),
        "custom_message" => usize::from(
            nonempty_string(object, "customType").is_some()
                && object
                    .get("content")
                    .cloned()
                    .and_then(|content| serde_json::from_value::<CustomMessageContent>(content).ok())
                    .is_some(),
        ),
        "branch_summary" => usize::from(nonempty_string(object, "summary").is_some()),
        "compaction" => usize::from(nonempty_string(object, "summary").is_some())
            .saturating_add(retained_tail.len()),
        _ => 0,
    }
}


fn build_tree_indexes(
    entries: &[SessionEntry],
    by_id: &HashMap<String, usize>,
) -> (
    HashMap<Option<String>, Vec<usize>>,
    HashMap<String, ResolvedLabel>,
) {
    let mut children = HashMap::<Option<String>, Vec<usize>>::new();
    let mut labels = HashMap::<String, ResolvedLabel>::new();
    for (index, entry) in entries.iter().enumerate() {
        let parent = entry
            .parent_id
            .as_ref()
            .filter(|parent| by_id.contains_key(*parent))
            .cloned();
        children.entry(parent).or_default().push(index);
        if entry.entry_type == "label"
            && let Some(target_id) = entry.target_id.as_ref()
        {
            if let Some(label) = entry.label.as_ref() {
                labels.insert(
                    target_id.clone(),
                    ResolvedLabel {
                        label: label.clone(),
                        timestamp: entry.timestamp.clone(),
                    },
                );
            } else {
                labels.remove(target_id);
            }
        }
    }
    for child_indexes in children.values_mut() {
        child_indexes.sort_by(|left, right| {
            entries[*left]
                .timestamp
                .cmp(&entries[*right].timestamp)
                .then_with(|| left.cmp(right))
        });
    }
    (children, labels)
}

pub fn load_session_messages(path: impl AsRef<Path>) -> Result<Vec<Message>> {
    Ok(load_session_tree(path)?.build_context(None).messages)
}

pub fn validate_session_id(id: &str) -> Result<()> {
    let valid = !id.is_empty()
        && id.chars().all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.'))
        && id.chars().next().is_some_and(|character| character.is_ascii_alphanumeric())
        && id.chars().next_back().is_some_and(|character| character.is_ascii_alphanumeric());
    if !valid {
        bail!("Session id must be non-empty, contain only alphanumeric characters, '-', '_', and '.', and start and end with an alphanumeric character");
    }
    Ok(())
}

#[must_use]
pub fn list_sessions_in(cwd: impl AsRef<Path>, session_dir: Option<&Path>) -> Vec<SessionInfo> {
    let cwd = absolute_path(cwd.as_ref());
    let directory = session_dir.map_or_else(|| default_session_dir(&cwd), absolute_path);
    let filter_cwd = session_dir.is_some();
    let Ok(read_dir) = fs::read_dir(directory) else { return Vec::new(); };
    // Pair each parsed session with its file last-modified time so the listing
    // can be ordered by mtime (matching upstream Pi), falling back to the
    // session timestamp and finally the path for determinism when metadata is
    // unavailable or two files share an mtime.
    let mut sessions = read_dir
        .take(MAX_SESSION_RECORDS)
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|extension| extension == "jsonl"))
        .filter_map(|entry| {
            let path = entry.path();
            let modified = fs::metadata(&path).ok().and_then(|metadata| metadata.modified().ok());
            read_session_info(&path).ok().map(|session| (session, modified))
        })
        .filter(|(session, _)| !filter_cwd || absolute_path(&session.cwd) == cwd)
        .collect::<Vec<_>>();
    sessions.sort_by(compare_session_listing);
    sessions.into_iter().map(|(session, _)| session).collect()
}

fn compare_session_listing(
    left: &(SessionInfo, Option<std::time::SystemTime>),
    right: &(SessionInfo, Option<std::time::SystemTime>),
) -> std::cmp::Ordering {
    match (left.1, right.1) {
        (Some(left_mtime), Some(right_mtime)) => right_mtime.cmp(&left_mtime),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
    .then_with(|| right.0.timestamp.cmp(&left.0.timestamp))
    .then_with(|| left.0.path.cmp(&right.0.path))
}

#[must_use]
pub fn list_sessions(cwd: impl AsRef<Path>) -> Vec<SessionInfo> {
    list_sessions_in(cwd, None)
}

#[must_use]
pub fn latest_session(cwd: impl AsRef<Path>) -> Option<SessionInfo> {
    list_sessions_in(cwd, None).into_iter().next()
}

/// Rename a saved session after proving the target is a regular JSONL file
/// directly inside the session root for `cwd`.
pub fn rename_saved_session(
    cwd: impl AsRef<Path>,
    path: impl AsRef<Path>,
    name: &str,
) -> Result<Option<String>> {
    let path = validated_saved_session_path(&default_session_dir(cwd), path.as_ref())?;
    let recorder = resume_session(&path)?;
    let normalized = recorder.record_session_name(name)?;
    recorder.close()?;
    Ok(normalized)
}

/// Permanently delete a saved session after proving the target is a regular
/// JSONL file directly inside the session root for `cwd`.
pub fn delete_saved_session(cwd: impl AsRef<Path>, path: impl AsRef<Path>) -> Result<()> {
    let path = validated_saved_session_path(&default_session_dir(cwd), path.as_ref())?;
    let metadata = fs::symlink_metadata(&path)
        .with_context(|| format!("reading session metadata {}", path.display()))?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        bail!("refusing to delete non-regular session file {}", path.display());
    }
    fs::remove_file(&path).with_context(|| format!("deleting saved session {}", path.display()))
}

pub fn validated_saved_session_path(root: &Path, path: &Path) -> Result<PathBuf> {
    if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl") {
        bail!("refusing saved-session mutation outside a .jsonl file: {}", path.display());
    }
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("reading session metadata {}", path.display()))?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        bail!("refusing saved-session mutation through non-regular file {}", path.display());
    }
    let root = root.to_path_buf();
    let canonical_root = fs::canonicalize(&root)
        .with_context(|| format!("resolving session root {}", root.display()))?;
    let canonical_path = fs::canonicalize(path)
        .with_context(|| format!("resolving saved session {}", path.display()))?;
    if canonical_path.parent() != Some(canonical_root.as_path()) {
        bail!(
            "refusing saved-session mutation outside session root {}: {}",
            canonical_root.display(),
            canonical_path.display()
        );
    }
    Ok(canonical_path)
}

pub(crate) fn normalize_session_name(name: &str) -> Option<String> {
    let mut normalized = String::with_capacity(name.len());
    let mut replacing_line_break = false;
    for character in name.chars() {
        if matches!(character, '\r' | '\n') {
            if !replacing_line_break {
                normalized.push(' ');
                replacing_line_break = true;
            }
        } else {
            normalized.push(character);
            replacing_line_break = false;
        }
    }
    let normalized = normalized.trim();
    (!normalized.is_empty()).then(|| normalized.to_owned())
}

fn append_token_from_state(state: &RecorderState) -> SessionAppendToken {
    SessionAppendToken {
        active_leaf_id: state.active_leaf_id.clone(),
        revision: state.revision,
    }
}

fn session_tree_from_state(state: &RecorderState) -> Result<SessionTree> {
    let mut tree = if state.flushed {
        let persisted = if let Some(file) = state.file.as_ref() {
            load_session_tree_from_file(
                file.try_clone().context("cloning attached session handle for tree load")?,
                &state.path,
            )?
        } else {
            load_session_tree(&state.path)?
        };
        if state.pending.is_empty() {
            persisted
        } else {
            let mut values = vec![json!({
                "type": "session",
                "version": persisted.header.version,
                "id": persisted.header.id,
                "timestamp": persisted.header.timestamp,
                "cwd": persisted.header.cwd,
                "parentSession": persisted.header.parent_session,
            })];
            values.extend(
                persisted
                    .entries
                    .iter()
                    .map(serde_json::to_value)
                    .collect::<std::result::Result<Vec<_>, _>>()?,
            );
            values.extend(state.pending.iter().cloned());
            session_tree_from_values(&state.path, &values)?
        }
    } else {
        session_tree_from_values(&state.path, &state.pending)?
    };
    tree.active_leaf_id.clone_from(&state.active_leaf_id);
    Ok(tree)
}

fn append_entry(state: &mut RecorderState, entry_type: &str, fields: Value) -> Result<String> {
    let previous_last_id = state.last_id.clone();
    let previous_active_leaf_id = state.active_leaf_id.clone();
    let previous_pending_len = state.pending.len();
    let id = prepare_entry(state, entry_type, fields);
    if let Err(error) = persist(state) {
        state.last_id = previous_last_id;
        state.active_leaf_id = previous_active_leaf_id;
        state.used_ids.remove(&id);
        state.pending.truncate(previous_pending_len);
        return Err(error);
    }
    state.revision = state.revision.saturating_add(1);
    Ok(id)
}

fn append_entry_durable(
    state: &mut RecorderState,
    entry_type: &str,
    fields: Value,
) -> Result<String> {
    let previous_last_id = state.last_id.clone();
    let previous_active_leaf_id = state.active_leaf_id.clone();
    let previous_pending_len = state.pending.len();
    let previous_file_len = state
        .file
        .as_ref()
        .map(File::metadata)
        .transpose()
        .context("reading session length before durable append")?
        .map(|metadata| metadata.len());
    let id = prepare_entry(state, entry_type, fields);
    if let Err(error) = persist_durable(state) {
        state.last_id = previous_last_id;
        state.active_leaf_id = previous_active_leaf_id;
        state.used_ids.remove(&id);
        state.pending.truncate(previous_pending_len);
        if let (Some(file), Some(previous_file_len)) = (state.file.as_mut(), previous_file_len) {
            file.set_len(previous_file_len)
                .and_then(|()| file.sync_all())
                .with_context(|| {
                    format!(
                        "rolling back durable session append after error: {error:#}"
                    )
                })?;
        }
        return Err(error);
    }
    state.revision = state.revision.saturating_add(1);
    Ok(id)
}

fn prepare_entry(state: &mut RecorderState, entry_type: &str, fields: Value) -> String {
    let id = unique_entry_id(&state.used_ids);
    let parent_id = state.active_leaf_id.clone();
    state.used_ids.insert(id.clone());
    let mut entry = match fields {
        Value::Object(object) => object,
        _ => Map::new(),
    };
    entry.insert("type".to_owned(), Value::String(entry_type.to_owned()));
    entry.insert("id".to_owned(), Value::String(id.clone()));
    entry.insert(
        "parentId".to_owned(),
        parent_id.map_or(Value::Null, Value::String),
    );
    entry.insert("timestamp".to_owned(), Value::String(iso_now()));
    state.last_id = Some(id.clone());
    state.active_leaf_id = Some(id.clone());
    state.pending.push(Value::Object(entry));
    id
}


fn persist_durable(state: &mut RecorderState) -> Result<()> {
    if !state.flushed {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .append(true)
            .create_new(true)
            .open(&state.path)
            .with_context(|| format!("creating session {}", state.path.display()))?;
        if let Err(error) = write_records_durable(&mut file, &state.pending) {
            drop(file);
            let _ = fs::remove_file(&state.path);
            return Err(error);
        }
        state.file = Some(file);
        state.flushed = true;
        state.pending.clear();
        return Ok(());
    }
    if let Some(file) = state.file.as_mut() {
        write_records_durable(file, &state.pending)?;
        state.pending.clear();
    }
    Ok(())
}

fn persist(state: &mut RecorderState) -> Result<()> {
    // Durable recorders persist every entry from the header onward, so they
    // do not wait for the first assistant message before writing to disk.
    if !state.durable && !state.has_assistant {
        return Ok(());
    }
    if !state.flushed {
        let mut file = OpenOptions::new()
            .write(true)
            .read(true)
            .append(true)
            .create_new(true)
            .open(&state.path)
            .with_context(|| format!("creating session {}", state.path.display()))?;
        let write_result = if state.durable {
            write_records_durable(&mut file, &state.pending)
        } else {
            write_records(&mut file, &state.pending)
        };
        if let Err(error) = write_result {
            drop(file);
            let _ = fs::remove_file(&state.path);
            return Err(error);
        }
        state.file = Some(file);
        state.flushed = true;
        state.pending.clear();
        return Ok(());
    }
    if let Some(file) = state.file.as_mut() {
        if state.durable {
            write_records_durable(file, &state.pending)?;
        } else {
            write_records(file, &state.pending)?;
        }
        state.pending.clear();
    }
    Ok(())
}

trait RecordAppendSink: Write {
    fn append_len(&self) -> io::Result<u64>;
    fn truncate_append(&mut self, len: u64) -> io::Result<()>;
    fn sync_rollback(&mut self) -> io::Result<()>;
}

impl RecordAppendSink for File {
    fn append_len(&self) -> io::Result<u64> {
        Ok(self.metadata()?.len())
    }

    fn truncate_append(&mut self, len: u64) -> io::Result<()> {
        self.set_len(len)
    }

    fn sync_rollback(&mut self) -> io::Result<()> {
        self.sync_all()
    }
}

fn write_records(file: &mut File, records: &[Value]) -> Result<()> {
    let mut serialized = Vec::new();
    for record in records {
        serde_json::to_writer(&mut serialized, record).context("serializing session record")?;
        serialized.push(b'\n');
    }
    append_serialized_records(file, &serialized).context("writing session record")
}

fn write_records_durable(file: &mut File, records: &[Value]) -> Result<()> {
    let mut serialized = Vec::new();
    for record in records {
        serde_json::to_writer(&mut serialized, record).context("serializing session record")?;
        serialized.push(b'\n');
    }
    append_serialized_records_durable(file, &serialized).context("writing durable session record")
}

fn append_serialized_records_durable<S: RecordAppendSink>(
    sink: &mut S,
    serialized: &[u8],
) -> io::Result<()> {
    let previous_len = sink.append_len()?;
    let write_result = sink
        .write_all(serialized)
        .and_then(|()| sink.flush())
        .and_then(|()| sink.sync_rollback());
    if let Err(write_error) = write_result {
        if let Err(rollback_error) = sink
            .truncate_append(previous_len)
            .and_then(|()| sink.sync_rollback())
        {
            return Err(io::Error::new(
                write_error.kind(),
                format!("{write_error}; rolling back partial session append failed: {rollback_error}"),
            ));
        }
        return Err(write_error);
    }
    Ok(())
}

fn append_serialized_records<S: RecordAppendSink>(sink: &mut S, serialized: &[u8]) -> io::Result<()> {
    let previous_len = sink.append_len()?;
    let write_result = sink.write_all(serialized).and_then(|()| sink.flush());
    if let Err(write_error) = write_result {
        if let Err(rollback_error) = sink
            .truncate_append(previous_len)
            .and_then(|()| sink.sync_rollback())
        {
            return Err(io::Error::new(
                write_error.kind(),
                format!("{write_error}; rolling back partial session append failed: {rollback_error}"),
            ));
        }
        return Err(write_error);
    }
    Ok(())
}

fn read_session_info(path: &Path) -> Result<SessionInfo> {
    let tree = load_session_tree(path)?;
    let session_name = tree.session_name();
    let message_text = tree
        .entries
        .iter()
        .filter_map(|entry| entry.message.as_ref())
        .map(message_search_text)
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>();
    Ok(SessionInfo {
        path: path.to_path_buf(),
        id: tree.header.id,
        cwd: tree.header.cwd,
        timestamp: tree.header.timestamp,
        name: session_name,
        messages: tree
            .entries
            .iter()
            .filter(|entry| entry.entry_type == "message")
            .count(),
        first_message: message_text.first().cloned().unwrap_or_else(|| "(no messages)".to_owned()),
        all_messages_text: message_text.join("\n"),
    })
}

fn message_search_text(message: &Message) -> String {
    match message {
        Message::User(message) => content_search_text(&message.content),
        Message::Assistant(message) => content_search_text(&message.content),
        Message::ToolResult(message) => content_search_text(&message.content),
        Message::BashExecution(message) => format!("{}\n{}", message.command, message.output),
        Message::Custom(message) if !message.display => String::new(),
        Message::Custom(message) => content_search_text(&message.content.to_blocks()),
        Message::BranchSummary(message) => message.summary.clone(),
        Message::CompactionSummary(message) => message.summary.clone(),
    }
}

fn content_search_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(text.clone()),
            ContentBlock::Thinking { thinking, .. } => Some(thinking.clone()),
            ContentBlock::ToolCall(call) => Some(format!("{} {}", call.name, call.arguments)),
            ContentBlock::Image { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
 }

fn append_entry_message(messages: &mut Vec<Message>, entry: &SessionEntry) {
    match entry.entry_type.as_str() {
        "message" => {
            if let Some(message) = &entry.message {
                messages.push(message.clone());
            }
        }
        "custom_message" => {
            if let (Some(custom_type), Some(content)) =
                (entry.custom_type.clone(), entry.content.clone())
            {
                messages.push(Message::Custom(CustomMessage {
                    custom_type,
                    content,
                    display: entry.display.unwrap_or(false),
                    details: entry.details.clone(),
                    timestamp: timestamp_millis(&entry.timestamp),
                }));
            }
        }
        "branch_summary" => {
            if let Some(summary) = entry.summary.as_deref() {
                messages.push(Message::BranchSummary(BranchSummaryMessage {
                    summary: summary.to_owned(),
                    from_id: entry.from_id.clone().unwrap_or_else(|| "root".to_owned()),
                    details: entry.details.clone(),
                    usage: entry.usage.clone(),
                    from_hook: entry.from_hook,
                    timestamp: timestamp_millis(&entry.timestamp),
                }));
            }
        }
        "compaction" => {
            if let Some(summary) = entry.summary.as_deref() {
                messages.push(Message::CompactionSummary(CompactionSummaryMessage {
                    summary: summary.to_owned(),
                    tokens_before: entry.tokens_before.unwrap_or_default(),
                    details: entry.details.clone(),
                    usage: entry.usage.clone(),
                    from_hook: entry.from_hook,
                    timestamp: timestamp_millis(&entry.timestamp),
                }));
            }
        }
        _ => {}
    }
}


fn timestamp_millis(timestamp: &str) -> i64 {
    DateTime::parse_from_rfc3339(timestamp)
        .map(|value| value.timestamp_millis())
        .unwrap_or_default()
}

fn nonempty_string(object: &Map<String, Value>, key: &str) -> Option<String> {
    object
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn unique_entry_id(used: &HashSet<String>) -> String {
    for _ in 0..100 {
        let id = Uuid::new_v4().simple().to_string()[..8].to_owned();
        if !used.contains(&id) {
            return id;
        }
    }
    Uuid::new_v4().to_string()
}

fn iso_now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn absolute_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

fn encode_cwd_safe_path(path: &Path) -> String {
    let mut encoded = path.to_string_lossy().into_owned();
    if encoded.starts_with('/') || encoded.starts_with('\\') {
        encoded.remove(0);
    }
    encoded.replace(['/', '\\', ':'], "-")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hidden_custom_messages_are_excluded_from_selector_search_text() {
        let hidden = Message::Custom(CustomMessage {
            custom_type: crate::LOOP_SCHEDULED_MESSAGE_TYPE.to_owned(),
            content: "<system-reminder>internal</system-reminder>".into(),
            display: false,
            details: None,
            timestamp: 1,
        });
        let visible = Message::Custom(CustomMessage {
            custom_type: "visible".to_owned(),
            content: "public note".into(),
            display: true,
            details: None,
            timestamp: 2,
        });
        assert!(message_search_text(&hidden).is_empty());
        assert_eq!(message_search_text(&visible), "public note");
    }


    fn session_header_line(cwd: &Path) -> String {
        serde_json::to_string(&json!({
            "type": "session",
            "version": CURRENT_SESSION_VERSION,
            "id": "bounded-session",
            "timestamp": "2026-01-01T00:00:00.000Z",
            "cwd": cwd,
        }))
        .expect("serialize session header")
    }

    fn native_session_body(cwd: &Path, id: &str, message: &str) -> String {
        format!(
            "{}\n{}\n",
            serde_json::to_string(&json!({
                "type": "session",
                "version": CURRENT_SESSION_VERSION,
                "id": id,
                "timestamp": "2026-01-01T00:00:00.000Z",
                "cwd": cwd,
            }))
            .expect("serialize header"),
            serde_json::to_string(&json!({
                "type": "message",
                "id": "first",
                "parentId": null,
                "timestamp": "2026-01-01T00:00:01.000Z",
                "message": Message::user_text(message, 0),
            }))
            .expect("serialize message")
        )
    }

    #[test]
    #[cfg(unix)]
    fn prepared_resume_rejects_final_component_symlink() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().expect("directory");
        let target = directory.path().join("target.jsonl");
        let link = directory.path().join("link.jsonl");
        fs::write(&target, native_session_body(directory.path(), "target", "target"))
            .expect("write target");
        symlink(&target, &link).expect("create symlink");

        let error = PreparedSessionResume::prepare_path(&link)
            .expect_err("final symlink must be rejected");
        assert!(format!("{error:#}").contains("no-follow"));
    }

    #[test]
    fn prepared_resume_rejects_malformed_and_oversized_inputs_without_mutation() {
        let directory = tempfile::tempdir().expect("directory");
        let malformed = directory.path().join("malformed.jsonl");
        let malformed_body = format!("{}\n{{not-json}}\n", session_header_line(directory.path()));
        fs::write(&malformed, &malformed_body).expect("write malformed");
        let error = PreparedSessionResume::prepare_path(&malformed)
            .expect_err("malformed session must fail");
        assert!(format!("{error:#}").contains("line 2"));
        assert_eq!(fs::read_to_string(&malformed).expect("read malformed"), malformed_body);

        let oversized = directory.path().join("oversized.jsonl");
        let file = File::create(&oversized).expect("create oversized");
        file.set_len(MAX_SESSION_FILE_BYTES + 1).expect("extend oversized");
        let error = PreparedSessionResume::prepare_path(&oversized)
            .expect_err("oversized session must fail");
        assert!(format!("{error:#}").contains("64 MiB file safety limit"));
        assert_eq!(fs::metadata(&oversized).expect("oversized metadata").len(), MAX_SESSION_FILE_BYTES + 1);
    }

    #[test]
    fn prepared_resume_retains_inode_across_path_replacement_for_tree_append_and_close() {
        let directory = tempfile::tempdir().expect("directory");
        let path = directory.path().join("session.jsonl");
        let held = directory.path().join("held.jsonl");
        let replacement_body = native_session_body(directory.path(), "replacement", "replacement");
        fs::write(&path, native_session_body(directory.path(), "held", "held"))
            .expect("write held session");

        let prepared = PreparedSessionResume::prepare_path(&path).expect("prepare held session");
        assert_eq!(prepared.target_cwd(), directory.path());
        assert_eq!(message_texts(&prepared.build_context()), vec!["held"]);
        fs::rename(&path, &held).expect("retain opened inode under another name");
        fs::write(&path, &replacement_body).expect("write replacement path");

        let recorder = prepared.into_recorder().expect("build retained recorder");
        assert_eq!(recorder.tree().expect("tree from retained handle").header.id, "held");
        let (tree, _) = recorder
            .tree_with_append_token()
            .expect("tree and token from retained handle");
        assert_eq!(tree.header.id, "held");
        recorder
            .record_message(&Message::user_text("appended", 0))
            .expect("append retained inode");
        recorder.close().expect("close retained inode");

        assert_eq!(
            message_texts(&load_session_tree(&held).expect("load held inode").build_context(None)),
            vec!["held", "appended"]
        );
        assert_eq!(fs::read_to_string(&path).expect("read replacement"), replacement_body);
    }

    #[test]
    fn persisted_recorder_retains_readable_handle_for_runtime_hydration() {
        let directory = tempfile::tempdir().expect("directory");
        let recorder = start_session_in(
            directory.path(),
            None,
            None,
            Some(directory.path()),
            None,
            None,
        )
        .expect("start session");
        recorder
            .record_message(&Message::user_text("hydrate", 0))
            .expect("record message");
        recorder.persist_now().expect("persist session");

        let (tree, _) = recorder
            .tree_with_append_token()
            .expect("read persisted recorder through retained handle");
        assert_eq!(message_texts(&tree.build_context(None)), vec!["hydrate"]);
    }

    #[test]
    fn session_loader_rejects_file_over_safety_limit_before_parsing() {
        let directory = tempfile::tempdir().expect("directory");
        let path = directory.path().join("oversized.jsonl");
        let file = File::create(&path).expect("create oversized session");
        file.set_len(MAX_SESSION_FILE_BYTES + 1).expect("extend oversized session");
        let error = load_session_tree(&path).expect_err("oversized file must fail");
        assert!(error.to_string().contains("64 MiB file safety limit"));
    }

    #[test]
    fn session_loader_rejects_line_over_safety_limit_with_context() {
        let directory = tempfile::tempdir().expect("directory");
        let path = directory.path().join("oversized-line.jsonl");
        let mut file = File::create(&path).expect("create session");
        writeln!(file, "{}", session_header_line(directory.path())).expect("write header");
        file.write_all(&vec![b'x'; MAX_SESSION_LINE_BYTES + 1]).expect("write oversized line");
        let error = load_session_tree(&path).expect_err("oversized line must fail");
        let message = format!("{error:#}");
        assert!(message.contains("line 2"));
        assert!(message.contains("8 MiB safety limit"));
    }

    #[test]
    fn session_loader_rejects_record_and_message_counts_above_limits() {
        let directory = tempfile::tempdir().expect("directory");
        let path = directory.path().join("bounded.jsonl");
        let mut records = Vec::with_capacity(MAX_SESSION_RECORDS + 1);
        records.push(json!({
            "type": "session",
            "version": CURRENT_SESSION_VERSION,
            "id": "bounded-session",
            "timestamp": "2026-01-01T00:00:00.000Z",
            "cwd": directory.path(),
        }));
        records.extend((0..MAX_SESSION_RECORDS).map(|index| json!({
            "type": "custom",
            "id": format!("entry-{index}"),
            "parentId": null,
            "timestamp": "2026-01-01T00:00:00.000Z",
            "customType": "state",
        })));
        let error = session_tree_from_values(&path, &records).expect_err("record limit must fail");
        assert!(error.to_string().contains("100000 record safety limit"));

        let retained = vec![Message::user_text("x", 0); MAX_RECONSTRUCTED_MESSAGES + 1];
        let values = vec![
            records[0].clone(),
            json!({
                "type": "compaction",
                "id": "compaction",
                "parentId": null,
                "timestamp": "2026-01-01T00:00:01.000Z",
                "summary": "summary",
                "retainedTail": retained,
            }),
        ];
        let error = session_tree_from_values(&path, &values).expect_err("message limit must fail");
        assert!(error.to_string().contains("100000 reconstructed message safety limit"));
    }

    #[test]
    fn bounded_session_load_still_resumes_and_appends_normally() {
        let directory = tempfile::tempdir().expect("directory");
        let path = directory.path().join("normal.jsonl");
        fs::write(
            &path,
            format!(
                "{}\n{}\n",
                session_header_line(directory.path()),
                serde_json::to_string(&json!({
                    "type": "message",
                    "id": "first",
                    "parentId": null,
                    "timestamp": "2026-01-01T00:00:01.000Z",
                    "message": Message::user_text("first", 0),
                }))
                .expect("serialize message")
            ),
        )
        .expect("write session");
        let recorder = resume_session(&path).expect("resume bounded session");
        recorder.record_message(&Message::user_text("second", 0)).expect("append message");
        recorder.close().expect("close recorder");
        assert_eq!(
            message_texts(&load_session_tree(&path).expect("reload").build_context(None)),
            vec!["first", "second"]
        );
    }

    struct FailingAppendSink {
        bytes: Vec<u8>,
        fail_after: usize,
        written: usize,
    }

    impl Write for FailingAppendSink {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            let remaining = self.fail_after.saturating_sub(self.written);
            if remaining == 0 {
                return Err(io::Error::new(io::ErrorKind::Other, "injected partial append"));
            }
            let written = remaining.min(buffer.len());
            self.bytes.extend_from_slice(&buffer[..written]);
            self.written += written;
            Ok(written)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl RecordAppendSink for FailingAppendSink {
        fn append_len(&self) -> io::Result<u64> {
            Ok(self.bytes.len() as u64)
        }

        fn truncate_append(&mut self, len: u64) -> io::Result<()> {
            self.bytes.truncate(len as usize);
            Ok(())
        }

        fn sync_rollback(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct FailingFlushSink {
        bytes: Vec<u8>,
        fail_flush: bool,
    }

    impl Write for FailingFlushSink {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.bytes.extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            if self.fail_flush {
                self.fail_flush = false;
                return Err(io::Error::new(io::ErrorKind::Other, "injected flush failure"));
            }
            Ok(())
        }
    }

    impl RecordAppendSink for FailingFlushSink {
        fn append_len(&self) -> io::Result<u64> {
            Ok(self.bytes.len() as u64)
        }

        fn truncate_append(&mut self, len: u64) -> io::Result<()> {
            self.bytes.truncate(len as usize);
            Ok(())
        }

        fn sync_rollback(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn durable_partial_write_and_flush_failures_restore_file_boundary() {
        let original = b"{\"type\":\"session\"}\n".to_vec();
        let mut partial = FailingAppendSink {
            bytes: original.clone(),
            fail_after: 8,
            written: 0,
        };
        append_serialized_records_durable(&mut partial, b"{\"type\":\"custom\"}\n")
            .expect_err("partial durable append must fail");
        assert_eq!(partial.bytes, original);

        let mut flush = FailingFlushSink {
            bytes: original.clone(),
            fail_flush: true,
        };
        append_serialized_records_durable(&mut flush, b"{\"type\":\"custom\"}\n")
            .expect_err("durable flush must fail");
        assert_eq!(flush.bytes, original);
    }

    #[test]
    fn failed_partial_append_restores_valid_jsonl_boundary() {
        let original = b"{\"type\":\"session\"}\n".to_vec();
        let mut sink = FailingAppendSink {
            bytes: original.clone(),
            fail_after: 8,
            written: 0,
        };
        let error = append_serialized_records(
            &mut sink,
            b"{\"type\":\"message\",\"id\":\"partial\"}\n",
        )
        .expect_err("partial append must fail");
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(sink.bytes, original);
        for line in sink.bytes.split(|byte| *byte == b'\n').filter(|line| !line.is_empty()) {
            serde_json::from_slice::<Value>(line).expect("remaining line is valid JSON");
        }
    }

    #[test]
    fn failed_first_flush_rolls_back_recorder_state() {
        let directory = std::env::temp_dir().join(format!("pi-session-store-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).expect("create test directory");
        let path = directory.join("session.jsonl");
        fs::write(&path, b"occupied").expect("occupy session path");
        let recorder = SessionRecorder {
            inner: Arc::new(Mutex::new(RecorderState {
                path: path.clone(),
                id: Uuid::now_v7().to_string(),
                timestamp: "2026-01-01T00:00:00.000Z".to_owned(),
                cwd: directory.clone(),
                parent_session: None,
                last_id: None,
                active_leaf_id: None,
                revision: 0,
                used_ids: HashSet::new(),
                pending: vec![json!({
                    "type": "session",
                    "version": CURRENT_SESSION_VERSION,
                    "id": "test-session",
                    "timestamp": "2026-01-01T00:00:00.000Z",
                    "cwd": directory,
                })],
                file: None,
                flushed: false,
                has_assistant: false,
                session_name: None,
                durable: false,
            })),
        };
        let assistant = Message::Assistant(pi_ai::AssistantMessage::pending(&Model::default()));

        assert!(recorder.record_message(&assistant).is_err());
        {
            let state = recorder.inner.lock();
            assert!(state.last_id.is_none());
            assert!(state.used_ids.is_empty());
            assert_eq!(state.pending.len(), 1);
            assert!(!state.has_assistant);
        }

        fs::remove_file(&path).expect("remove occupied path");
        recorder
            .record_message(&assistant)
            .expect("retry append after clearing failure");
        let records = fs::read_to_string(&path).expect("read persisted session");
        let message = records
            .lines()
            .nth(1)
            .map(|line| serde_json::from_str::<Value>(line).expect("parse message record"))
            .expect("message record");
        assert!(message["parentId"].is_null());
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn persist_now_writes_header_without_assistant() {
        let directory = std::env::temp_dir().join(format!("pi-session-persist-now-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).expect("create test directory");
        let recorder = start_session_with_parent(&directory, None, None, Some(Path::new("parent.jsonl"))).expect("start session");
        assert!(!recorder.path().exists());
        recorder.persist_now().expect("persist header");
        let tree = load_session_tree(recorder.path()).expect("load header-only session");
        assert_eq!(tree.header.parent_session.as_deref(), Some("parent.jsonl"));
        assert!(tree.entries.is_empty());
        recorder.close().expect("close recorder");
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    fn test_entry(id: &str, parent_id: Option<&str>) -> SessionEntry {
        SessionEntry {
            id: id.to_owned(),
            parent_id: parent_id.map(str::to_owned),
            entry_type: "message".to_owned(),
            timestamp: "2026-01-01T00:00:00.000Z".to_owned(),
            message: Some(Message::user_text(id.to_owned(), 0)),
            provider: None,
            model_id: None,
            thinking_level: None,
            summary: None,
            first_kept_entry_id: None,
            tokens_before: None,
            retained_tail: Vec::new(),
            usage: None,
            from_hook: None,
            content: None,
            name: None,
            label: None,
            target_id: None,
            from_id: None,
            custom_type: None,
            display: None,
            details: None,
            data: None,
            todo_state: None,
        }
    }

    fn test_tree(entries: Vec<SessionEntry>, active_leaf_id: Option<&str>) -> SessionTree {
        let mut by_id = HashMap::new();
        for (index, entry) in entries.iter().enumerate() {
            by_id.insert(entry.id.clone(), index);
        }
        let leaf_id = entries.last().map(|entry| entry.id.clone());
        let (children_by_parent, labels) = build_tree_indexes(&entries, &by_id);
        SessionTree {
            header: SessionHeader {
                record_type: "session".to_owned(),
                version: CURRENT_SESSION_VERSION,
                id: "test-session".to_owned(),
                timestamp: "2026-01-01T00:00:00.000Z".to_owned(),
                cwd: PathBuf::from("<workspace>"),
                parent_session: None,
            },
            entries,
            leaf_id,
            active_leaf_id: active_leaf_id.map(str::to_owned),
            by_id,
            children_by_parent,
            labels,
        }
    }

    fn branch_ids<'a>(tree: &'a SessionTree, leaf_id: Option<&str>) -> Vec<&'a str> {
        tree.branch(leaf_id)
            .iter()
            .map(|entry| entry.id.as_str())
            .collect()
    }

    fn message_texts(context: &BranchContext) -> Vec<String> {
        context
            .messages
            .iter()
            .filter_map(|message| match message {
                Message::User(user) => user.content.iter().find_map(|block| match block {
                    ContentBlock::Text { text, .. } => Some(text.clone()),
                    _ => None,
                }),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn default_branch_prefers_selected_leaf_over_last_appended_entry() {
        // Main line a -> b -> c; d forks from a and is appended last.
        // The selected leaf "b" is therefore not the last appended entry.
        let entries = vec![
            test_entry("a", None),
            test_entry("b", Some("a")),
            test_entry("c", Some("b")),
            test_entry("d", Some("a")),
        ];
        let tree = test_tree(entries, Some("b"));

        assert_eq!(branch_ids(&tree, None), vec!["a", "b"]);

        // An explicit argument still wins over the selected leaf.
        assert_eq!(branch_ids(&tree, Some("c")), vec!["a", "b", "c"]);
        assert_eq!(branch_ids(&tree, Some("d")), vec!["a", "d"]);

        // The default context reflects the selected branch, not the last append.
        assert_eq!(message_texts(&tree.build_context(None)), vec!["a", "b"]);
    }

    #[test]
    fn default_branch_is_empty_after_root_reset() {
        let entries = vec![
            test_entry("a", None),
            test_entry("b", Some("a")),
            test_entry("d", Some("a")),
        ];
        let tree = test_tree(entries, None);

        assert!(branch_ids(&tree, None).is_empty());
    }

    #[test]
    fn unknown_active_leaf_has_no_default_branch() {
        let entries = vec![test_entry("a", None), test_entry("b", Some("a"))];
        let tree = test_tree(entries, Some("missing"));

        assert!(tree.branch(None).is_empty());
        assert_eq!(branch_ids(&tree, Some("b")), vec!["a", "b"]);
    }

    #[test]
    fn resume_continues_and_reloads_the_active_branch() {
        let directory = std::env::temp_dir().join(format!("pi-session-resume-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).expect("create test directory");
        let path = directory.join("session.jsonl");
        let timestamp = "2026-01-01T00:00:00.000Z";

        // a -> b -> c is the main line; d forks from b and is appended last.
        let mut lines = vec![json!({
            "type": "session",
            "version": CURRENT_SESSION_VERSION,
            "id": "test-session",
            "timestamp": timestamp,
            "cwd": directory,
        })];
        for (id, parent_id) in [
            ("a", None),
            ("b", Some("a")),
            ("c", Some("b")),
            ("d", Some("b")),
        ] {
            lines.push(json!({
                "type": "message",
                "id": id,
                "parentId": parent_id,
                "timestamp": timestamp,
                "message": Message::user_text(id, 0),
            }));
        }
        let serialized = lines
            .iter()
            .map(|line| serde_json::to_string(line).expect("serialize record"))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&path, serialized).expect("write session file");

        let tree = load_session_tree(&path).expect("load session");
        assert_eq!(branch_ids(&tree, None), vec!["a", "b", "d"]);

        let recorder = resume_session(&path).expect("resume session");
        assert_eq!(recorder.last_entry_id().as_deref(), Some("d"));
        recorder
            .record_message(&Message::user_text("e", 0))
            .expect("record message");
        recorder.close().expect("close session");

        let reloaded = load_session_tree(&path).expect("reload session");
        assert_eq!(message_texts(&reloaded.build_context(None)), vec!["a", "b", "d", "e"]);

        fs::remove_dir_all(directory).expect("remove test directory");
    }
    #[test]
    fn malformed_jsonl_record_fails_load_and_resume() {
        for corrupt_record in ["{not-json}", "[]"] {
            let directory = std::env::temp_dir().join(format!("pi-session-corrupt-{}", Uuid::new_v4()));
            fs::create_dir_all(&directory).expect("create test directory");
            let path = directory.join("session.jsonl");
            let header = json!({
                "type": "session",
                "version": CURRENT_SESSION_VERSION,
                "id": "test-session",
                "timestamp": "2026-01-01T00:00:00.000Z",
                "cwd": directory,
            });
            fs::write(
                &path,
                format!(
                    "{}\n{corrupt_record}\n",
                    serde_json::to_string(&header).expect("serialize header")
                ),
            )
            .expect("write corrupt session");
            let original = fs::read(&path).expect("snapshot corrupt session");

            let load_error = load_session_tree(&path).expect_err("reject corrupt record");
            assert!(load_error.to_string().contains("line 2"));
            let resume_error = resume_session(&path).expect_err("reject corrupt resume");
            assert!(resume_error.to_string().contains("line 2"));
            assert_eq!(fs::read(&path).expect("read unchanged session"), original);

            fs::remove_dir_all(directory).expect("remove test directory");
        }
    }

    #[test]
    fn resume_separates_final_records_without_newline() {
        for suffix in ["", "\r"] {
            let directory = std::env::temp_dir().join(format!("pi-session-newline-{}", Uuid::new_v4()));
            fs::create_dir_all(&directory).expect("create test directory");
            let path = directory.join("session.jsonl");
            let header = serde_json::to_string(&json!({
                "type": "session",
                "version": CURRENT_SESSION_VERSION,
                "id": "test-session",
                "timestamp": "2026-01-01T00:00:00.000Z",
                "cwd": directory,
            }))
            .expect("serialize header");
            let first = serde_json::to_string(&json!({
                "type": "message",
                "id": "a",
                "parentId": null,
                "timestamp": "2026-01-01T00:00:01.000Z",
                "message": Message::user_text("first", 0),
            }))
            .expect("serialize first record");
            fs::write(&path, format!("{header}\n{first}{suffix}"))
                .expect("write incomplete final separator");

            let recorder = resume_session(&path).expect("resume session");
            recorder
                .record_message(&Message::user_text("second", 0))
                .expect("append message");
            recorder.close().expect("close recorder");

            let reloaded = load_session_tree(&path).expect("reload separated records");
            assert_eq!(
                message_texts(&reloaded.build_context(None)),
                vec!["first", "second"]
            );
            fs::remove_dir_all(directory).expect("remove test directory");
        }
    }
    #[test]
    fn public_session_record_label_clear_omits_optional_label() {
        let record = SessionRecord::Label {
            id: "label-id".to_owned(),
            parent_id: Some("entry-id".to_owned()),
            timestamp: "2026-01-01T00:00:00.000Z".to_owned(),
            target_id: "entry-id".to_owned(),
            label: None,
        };
        let encoded = serde_json::to_value(&record).expect("serialize session record");

        let branch = json!({
            "type":"branch_summary", "id":"branch-id", "parentId":"entry-id",
            "timestamp":"2026-01-01T00:00:01.000Z", "fromId":"entry-id", "summary":"branch",
            "details":{"readFiles":["src/lib.rs"]},
            "usage":{"input":1,"output":2,"cacheRead":0,"cacheWrite":0,"cacheWrite1h":0,"reasoning":0,"totalTokens":3,"cost":{"input":0.0,"output":0.0,"cacheRead":0.0,"cacheWrite":0.0,"total":0.0}},
            "fromHook":true
        });
        let branch_record: SessionRecord = serde_json::from_value(branch.clone()).expect("deserialize branch record");
        assert_eq!(serde_json::to_value(branch_record).expect("serialize branch record"), branch);

        let compaction = json!({
            "type":"compaction", "id":"compaction-id", "parentId":"branch-id",
            "timestamp":"2026-01-01T00:00:02.000Z", "summary":"compact", "tokensBefore":9,
            "details":{"modifiedFiles":["src/main.rs"]},
            "usage":{"input":3,"output":4,"cacheRead":0,"cacheWrite":0,"cacheWrite1h":0,"reasoning":0,"totalTokens":7,"cost":{"input":0.0,"output":0.0,"cacheRead":0.0,"cacheWrite":0.0,"total":0.0}},
            "fromHook":false
        });
        let compaction_record: SessionRecord = serde_json::from_value(compaction.clone()).expect("deserialize compaction record");
        assert_eq!(serde_json::to_value(compaction_record).expect("serialize compaction record"), compaction);
        assert_eq!(encoded["type"], "label");
        assert_eq!(encoded["parentId"], "entry-id");
        assert!(encoded.get("label").is_none());
        let decoded: SessionRecord = serde_json::from_value(encoded).expect("deserialize session record");
        assert_eq!(decoded, record);
    }

    #[test]
    fn summary_metadata_round_trips_entries_context_and_recorder() {
        let directory = tempfile::tempdir().expect("directory");
        let recorder = start_session_in(
            directory.path(), None, None, Some(directory.path()), Some("summary-metadata"), None,
        )
        .expect("start session");
        let root_id = recorder.record_message(&Message::user_text("root", 0)).expect("record root");
        let branch_details = json!({"readFiles":["src/lib.rs"]});
        let branch_usage = Usage { input: 11, output: 3, total_tokens: 14, ..Usage::default() };
        let branch_id = recorder
            .branch_with_summary_metadata(
                Some(&root_id), "branch summary", Some(&branch_details), Some(&branch_usage), Some(true),
            )
            .expect("record branch summary");
        let compaction_details = json!({"modifiedFiles":["src/main.rs"]});
        let compaction_usage = Usage { input: 21, output: 5, total_tokens: 26, ..Usage::default() };
        let compaction_id = recorder
            .record_compaction_metadata(
                "compaction summary", None, 1234, &[], Some(&compaction_details), Some(&compaction_usage), Some(false),
            )
            .expect("record compaction");
        recorder.close().expect("close recorder");

        let tree = load_session_tree(recorder.path()).expect("reload summaries");
        let branch_entry = tree.entries.iter().find(|entry| entry.id == branch_id).expect("branch entry");
        assert_eq!(branch_entry.details.as_ref(), Some(&branch_details));
        assert_eq!(branch_entry.usage.as_ref(), Some(&branch_usage));
        assert_eq!(branch_entry.from_hook, Some(true));
        assert!(matches!(tree.build_context(Some(&branch_id)).messages.last(),
            Some(Message::BranchSummary(message))
                if message.details.as_ref() == Some(&branch_details)
                    && message.usage.as_ref() == Some(&branch_usage)
                    && message.from_hook == Some(true)));

        let compaction_entry = tree.entries.iter().find(|entry| entry.id == compaction_id).expect("compaction entry");
        assert_eq!(compaction_entry.details.as_ref(), Some(&compaction_details));
        assert_eq!(compaction_entry.usage.as_ref(), Some(&compaction_usage));
        assert_eq!(compaction_entry.from_hook, Some(false));
        assert!(matches!(tree.build_context(Some(&compaction_id)).messages.first(),
            Some(Message::CompactionSummary(message))
                if message.details.as_ref() == Some(&compaction_details)
                    && message.usage.as_ref() == Some(&compaction_usage)
                    && message.from_hook == Some(false)));

        let legacy = start_session_in(
            directory.path(), None, None, Some(directory.path()), Some("summary-legacy"), None,
        )
        .expect("legacy recorder");
        legacy.record_compaction("legacy", None, 7, &[]).expect("legacy compaction");
        legacy.close().expect("close legacy recorder");
        let legacy_rows = fs::read_to_string(legacy.path()).expect("read legacy rows");
        let legacy_compaction = legacy_rows.lines()
            .map(|line| serde_json::from_str::<Value>(line).expect("parse legacy row"))
            .find(|row| row["type"] == "compaction")
            .expect("legacy compaction row");
        assert!(legacy_compaction.get("details").is_none());
        assert!(legacy_compaction.get("usage").is_none());
        assert!(legacy_compaction.get("fromHook").is_none());
    }

    #[test]
    fn create_branched_session_rechains_labels_and_recreates_resolved_labels() {
        let directory = tempfile::tempdir().expect("directory");
        let source_path = directory.path().join("source.jsonl");
        let lines = [
            json!({"type":"session","version":CURRENT_SESSION_VERSION,"id":"source","timestamp":"2026-01-01T00:00:00.000Z","cwd":directory.path()}),
            json!({"type":"message","id":"a","parentId":null,"timestamp":"2026-01-01T00:00:01.000Z","message":Message::user_text("a", 0)}),
            json!({"type":"label","id":"label-a","parentId":"a","timestamp":"2026-01-01T00:00:02.000Z","targetId":"a","label":"cleared"}),
            json!({"type":"message","id":"b","parentId":"label-a","timestamp":"2026-01-01T00:00:03.000Z","message":Message::user_text("b", 0)}),
            json!({"type":"label","id":"clear-a","parentId":"b","timestamp":"2026-01-01T00:00:04.000Z","targetId":"a"}),
            json!({"type":"label","id":"old-b","parentId":"clear-a","timestamp":"2026-01-01T00:00:05.000Z","targetId":"b","label":"old"}),
            json!({"type":"label","id":"new-b","parentId":"old-b","timestamp":"2026-01-01T00:00:06.000Z","targetId":"b","label":"current"}),
            json!({"type":"message","id":"c","parentId":"new-b","timestamp":"2026-01-01T00:00:07.000Z","message":Message::user_text("c", 0)}),
            json!({"type":"label","id":"label-c","parentId":"c","timestamp":"2026-01-01T00:00:08.000Z","targetId":"c","label":"leaf"}),
        ];
        fs::write(
            &source_path,
            lines.iter().map(|line| serde_json::to_string(line).expect("serialize source row")).collect::<Vec<_>>().join("\n"),
        )
        .expect("write source");

        let fork = create_branched_session(&source_path, "c").expect("create branch");
        let fork_path = fork.path();
        fork.close().expect("close branch");
        let tree = load_session_tree(&fork_path).expect("load branch");
        assert_eq!(tree.tree().len(), 1);
        assert_eq!(tree.active_leaf_id.as_deref(), Some("c"));
        assert_eq!(message_texts(&tree.build_context(None)), vec!["a", "b", "c"]);

        let normal = tree.entries.iter().filter(|entry| entry.entry_type != "label").collect::<Vec<_>>();
        assert_eq!(normal.iter().map(|entry| entry.id.as_str()).collect::<Vec<_>>(), ["a", "b", "c"]);
        assert_eq!(normal[0].parent_id, None);
        assert_eq!(normal[1].parent_id.as_deref(), Some("a"));
        assert_eq!(normal[2].parent_id.as_deref(), Some("b"));

        let labels = tree.entries.iter().filter(|entry| entry.entry_type == "label").collect::<Vec<_>>();
        assert_eq!(labels.len(), 2);
        assert!(labels.iter().all(|entry| !matches!(entry.id.as_str(), "label-a" | "clear-a" | "old-b" | "new-b" | "label-c")));
        let all_ids = tree.entries.iter().map(|entry| entry.id.as_str()).collect::<HashSet<_>>();
        assert_eq!(all_ids.len(), tree.entries.len());
        for pair in tree.entries.windows(2) {
            assert_eq!(pair[1].parent_id.as_deref(), Some(pair[0].id.as_str()));
        }
        assert_eq!(tree.labels.get("a").map(|label| label.label.as_str()), None);
        assert_eq!(tree.labels.get("b").map(|label| label.label.as_str()), Some("current"));
        assert_eq!(tree.labels.get("b").map(|label| label.timestamp.as_str()), Some("2026-01-01T00:00:06.000Z"));
        assert_eq!(tree.labels.get("c").map(|label| label.label.as_str()), Some("leaf"));
        assert_eq!(tree.labels.get("c").map(|label| label.timestamp.as_str()), Some("2026-01-01T00:00:08.000Z"));
    }

    #[test]
    fn custom_records_omit_absent_optional_metadata() {
        let directory = std::env::temp_dir().join(format!("pi-session-custom-omit-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).expect("create test directory");
        let recorder = start_session(&directory, None, None).expect("start session");
        recorder.record_custom_entry("extension.state", None).expect("record custom state");
        recorder.record_custom_message(&CustomMessage {
            custom_type: "extension.notice".into(),
            content: "visible".into(),
            display: true,
            details: None,
            timestamp: 123,
        }).expect("record custom message");
        recorder
            .record_message(&Message::Assistant(pi_ai::AssistantMessage::pending(&Model::default())))
            .expect("flush session");
        recorder.close().expect("close recorder");

        let records = fs::read_to_string(recorder.path()).expect("read session");
        let rows = records.lines().map(|line| serde_json::from_str::<Value>(line).expect("parse row")).collect::<Vec<_>>();
        let state = rows.iter().find(|row| row["type"] == "custom").expect("custom row");
        assert!(state.get("data").is_none());
        let message = rows.iter().find(|row| row["type"] == "custom_message").expect("custom message row");
        assert!(message.get("details").is_none());
        assert!(message.get("role").is_none());
        assert!(message["timestamp"].as_str().is_some());
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn custom_entries_round_trip_and_project_with_original_metadata() {
        let directory = std::env::temp_dir().join(format!("pi-session-custom-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).expect("create test directory");
        let recorder = start_session(&directory, None, None).expect("start session");
        recorder
            .record_custom_entry("extension.state", Some(json!({"cursor":7})))
            .expect("record custom state");
        let custom = CustomMessage {
            custom_type: "extension.notice".into(),
            content: CustomMessageContent::Blocks(vec![ContentBlock::text("remember"), ContentBlock::Image {
                data: "aGVsbG8=".into(), mime_type: "image/png".into(),
            }]),
            display: false,
            details: Some(json!({"nested":{"preserved":true}})),
            timestamp: 123,
        };
        recorder.record_custom_message(&custom).expect("record custom message");
        recorder
            .record_message(&Message::Assistant(pi_ai::AssistantMessage::pending(&Model::default())))
            .expect("flush session");
        recorder.close().expect("close recorder");

        let tree = load_session_tree(recorder.path()).expect("reload custom session");
        let state = tree.entries.iter().find(|entry| entry.entry_type == "custom").expect("custom state entry");
        assert_eq!(state.custom_type.as_deref(), Some("extension.state"));
        assert_eq!(state.data, Some(json!({"cursor":7})));
        let entry = tree.entries.iter().find(|entry| entry.entry_type == "custom_message").expect("custom message entry");
        assert_eq!(entry.custom_type.as_deref(), Some("extension.notice"));
        assert_eq!(entry.display, Some(false));
        assert_eq!(entry.details, custom.details);
        assert_eq!(entry.content, Some(custom.content.clone()));
        assert!(matches!(&tree.build_context(None).messages[0], Message::Custom(message)
            if message.custom_type == custom.custom_type
                && message.content == custom.content
                && !message.display
                && message.details == custom.details));
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn session_name_normalizes_persists_and_clears() {
        let directory = std::env::temp_dir().join(format!("pi-session-name-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).expect("create test directory");
        let path = directory.join("session.jsonl");
        let header = json!({
            "type": "session",
            "version": CURRENT_SESSION_VERSION,
            "id": "named-session",
            "timestamp": "2026-01-01T00:00:00.000Z",
            "cwd": directory,
        });
        fs::write(&path, format!("{}\n", serde_json::to_string(&header).expect("header")))
            .expect("write session");

        let recorder = resume_session(&path).expect("resume recorder");
        assert_eq!(
            recorder.record_session_name("  Alpha\r\nBeta  ").expect("record name").as_deref(),
            Some("Alpha Beta")
        );
        assert_eq!(recorder.session_name().as_deref(), Some("Alpha Beta"));
        assert_eq!(load_session_tree(&path).expect("load named").session_name().as_deref(), Some("Alpha Beta"));

        assert_eq!(recorder.record_session_name(" \n ").expect("clear name"), None);
        assert_eq!(recorder.session_name(), None);
        assert_eq!(load_session_tree(&path).expect("load cleared").session_name(), None);
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn bash_execution_persists_and_loads_without_role_conversion() {
        let directory = std::env::temp_dir().join(format!("pi-session-bash-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).expect("create test directory");
        let path = directory.join("session.jsonl");
        let bash = Message::BashExecution(pi_ai::BashExecutionMessage {
            command: "echo saved".into(), output: "saved".into(), exit_code: Some(0),
            cancelled: false, truncated: false, full_output_path: None,
            timestamp: 42, exclude_from_context: Some(true),
        });
        let lines = [
            json!({"type":"session", "version":CURRENT_SESSION_VERSION, "id":"test-session", "timestamp":"2026-01-01T00:00:00.000Z", "cwd":directory}),
            json!({"type":"message", "id":"a", "parentId":null, "timestamp":"2026-01-01T00:00:01.000Z", "message":bash}),
        ];
        fs::write(
            &path,
            lines.iter().map(|line| serde_json::to_string(line).expect("serialize record")).collect::<Vec<_>>().join("\n"),
        ).expect("write session");

        let tree = load_session_tree(&path).expect("load session");
        assert!(matches!(tree.entries[0].message, Some(Message::BashExecution(ref message))
            if message.command == "echo saved" && message.exclude_from_context == Some(true)));
        assert_eq!(tree.build_context(None).messages, vec![bash]);
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn saved_session_mutations_stay_inside_session_root() {
        let root = tempfile::tempdir().expect("session root");
        let path = root.path().join("saved.jsonl");
        fs::write(&path, "saved").expect("write session");

        assert_eq!(
            validated_saved_session_path(root.path(), &path).expect("validate"),
            fs::canonicalize(&path).expect("canonical saved path")
        );
        let outside_root = tempfile::tempdir().expect("outside root");
        let outside = outside_root.path().join("outside.jsonl");
        fs::write(&outside, "outside").expect("outside file");
        assert!(validated_saved_session_path(root.path(), &outside).is_err());
        assert!(outside.exists());
    }

    #[cfg(unix)]
    #[test]
    fn saved_session_delete_rejects_symlink_targets() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("session root");
        let outside_root = tempfile::tempdir().expect("outside root");
        let outside = outside_root.path().join("outside.jsonl");
        fs::write(&outside, "outside").expect("outside file");
        let alias = root.path().join("alias.jsonl");
        symlink(&outside, &alias).expect("symlink");

        assert!(validated_saved_session_path(root.path(), &alias).is_err());
        assert!(outside.exists());
    }

    #[test]
    fn explicit_session_directory_supports_ids_and_forks() {
        let cwd = tempfile::tempdir().expect("cwd");
        let sessions = tempfile::tempdir().expect("sessions");
        let recorder = start_session_in(
            cwd.path(),
            None,
            Some("high"),
            Some(sessions.path()),
            Some("exact_id"),
            None,
        )
        .expect("start explicit session");
        let source = recorder.path().to_path_buf();
        recorder.close().expect("close source");
        assert_eq!(list_sessions_in(cwd.path(), Some(sessions.path()))[0].id, "exact_id");

        let forked = fork_session_in(
            &source,
            cwd.path(),
            Some(sessions.path()),
            Some("fork_id"),
        )
        .expect("fork explicit session");
        assert_eq!(forked.id(), "fork_id");
        forked.close().expect("close fork");

        let source_recorder = resume_session(&source).expect("resume source for labels");
        let leaf = source_recorder
            .record_message(&Message::user_text("labeled leaf", 2))
            .expect("record labeled leaf");
        source_recorder.record_label(&leaf, Some("checkpoint")).expect("label leaf");
        source_recorder.close().expect("close labeled source");
        let labeled_fork = fork_session_in(
            &source,
            cwd.path(),
            Some(sessions.path()),
            Some("fork_labels"),
        )
        .expect("fork labeled session");
        labeled_fork.close().expect("close labeled fork");
        let fork_tree = load_session_tree(labeled_fork.path()).expect("load labeled fork");
        assert_eq!(fork_tree.active_leaf_id.as_deref(), Some(leaf.as_str()));
        assert_eq!(fork_tree.labels.get(&leaf).map(|label| label.label.as_str()), Some("checkpoint"));
        assert_eq!(fork_tree.tree().len(), 1);
        for pair in fork_tree.entries.windows(2) {
            assert_eq!(pair[1].parent_id.as_deref(), Some(pair[0].id.as_str()));
        }
        assert_eq!(list_sessions_in(cwd.path(), Some(sessions.path())).len(), 3);
    }

    #[test]
    fn session_ids_reject_path_syntax_and_duplicates() {
        for invalid in ["", ".hidden", "../escape", "bad/name", "bad\\name", "space id"] {
            assert!(validate_session_id(invalid).is_err(), "accepted {invalid:?}");
        }
        let cwd = tempfile::tempdir().expect("cwd");
        let sessions = tempfile::tempdir().expect("sessions");
        let recorder = start_session_in(
            cwd.path(),
            None,
            None,
            Some(sessions.path()),
            Some("duplicate"),
            None,
        )
        .expect("start session");
        recorder.close().expect("close session");
        assert!(start_session_in(
            cwd.path(),
            None,
            None,
            Some(sessions.path()),
            Some("duplicate"),
            None,
        )
        .is_err());
    }

    #[test]
    fn session_listing_prefers_real_mtime_and_uses_total_fallback_order() {
        use std::time::{Duration, SystemTime};

        let directory = tempfile::tempdir().expect("session directory");
        let cwd = tempfile::tempdir().expect("cwd");
        let older_path = directory.path().join("older-timestamp-newer-mtime.jsonl");
        let newer_path = directory.path().join("newer-timestamp-older-mtime.jsonl");
        let older_body = format!(
            "{}\n",
            serde_json::to_string(&json!({
                "type":"session", "version":CURRENT_SESSION_VERSION, "id":"mtime-wins",
                "timestamp":"2026-01-01T00:00:01.000Z", "cwd":cwd.path()
            }))
            .expect("serialize older timestamp")
        );
        let newer_body = format!(
            "{}\n",
            serde_json::to_string(&json!({
                "type":"session", "version":CURRENT_SESSION_VERSION, "id":"timestamp-loses",
                "timestamp":"2026-01-01T00:00:02.000Z", "cwd":cwd.path()
            }))
            .expect("serialize newer timestamp")
        );
        fs::write(&older_path, &older_body).expect("write first session");
        fs::write(&newer_path, newer_body).expect("write second session");
        let second_mtime = fs::metadata(&newer_path).expect("second metadata").modified().expect("second mtime");
        for _ in 0..100 {
            std::thread::sleep(Duration::from_millis(2));
            fs::write(&older_path, &older_body).expect("touch first session");
            if fs::metadata(&older_path).expect("first metadata").modified().expect("first mtime") > second_mtime {
                break;
            }
        }
        assert!(
            fs::metadata(&older_path).expect("first metadata").modified().expect("first mtime") > second_mtime,
            "test filesystem did not advance mtime"
        );
        let listed = list_sessions_in(cwd.path(), Some(directory.path()));
        assert_eq!(listed.iter().map(|session| session.id.as_str()).collect::<Vec<_>>(), ["mtime-wins", "timestamp-loses"]);

        let first = listed[0].clone();
        let second = listed[1].clone();
        let mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(10);
        assert_eq!(
            compare_session_listing(&(first.clone(), Some(mtime)), &(second.clone(), None)),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            compare_session_listing(&(first, None), &(second, None)),
            std::cmp::Ordering::Greater
        );
    }

    #[test]
    fn navigation_creates_branches_and_root_resets_without_rewriting_history() {
        let directory = std::env::temp_dir().join(format!("pi-session-tree-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).expect("create test directory");
        let path = directory.join("session.jsonl");
        let lines = [
            json!({"type":"session", "version":CURRENT_SESSION_VERSION, "id":"tree-session", "timestamp":"2026-01-01T00:00:00.000Z", "cwd":directory}),
            json!({"type":"message", "id":"a", "parentId":null, "timestamp":"2026-01-01T00:00:01.000Z", "message":Message::user_text("root", 0)}),
            json!({"type":"message", "id":"b", "parentId":"a", "timestamp":"2026-01-01T00:00:02.000Z", "message":Message::user_text("first branch", 0)}),
        ];
        fs::write(&path, lines.iter().map(|line| serde_json::to_string(line).expect("serialize record")).collect::<Vec<_>>().join("\n")).expect("write session");
        let recorder = resume_session(&path).expect("resume");
        recorder.branch("a").expect("move to root entry");
        let branch_id = recorder.record_message(&Message::user_text("second branch", 0)).expect("append branch");
        recorder.reset_leaf();
        let root_id = recorder.record_message(&Message::user_text("second root", 0)).expect("append root");
        recorder.close().expect("close recorder");
        let tree = load_session_tree(&path).expect("reload");
        assert_eq!(tree.entries.iter().find(|entry| entry.id == branch_id).and_then(|entry| entry.parent_id.as_deref()), Some("a"));
        assert_eq!(tree.entries.iter().find(|entry| entry.id == root_id).and_then(|entry| entry.parent_id.as_deref()), None);
        assert_eq!(tree.tree().len(), 2);
        assert_eq!(fs::read_to_string(&path).expect("read file").lines().count(), 5);
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn labels_are_append_only_resolved_and_clearable() {
        let directory = std::env::temp_dir().join(format!("pi-session-label-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).expect("create test directory");
        let path = directory.join("session.jsonl");
        let lines = [
            json!({"type":"session", "version":CURRENT_SESSION_VERSION, "id":"label-session", "timestamp":"2026-01-01T00:00:00.000Z", "cwd":directory}),
            json!({"type":"message", "id":"a", "parentId":null, "timestamp":"2026-01-01T00:00:01.000Z", "message":Message::user_text("root", 0)}),
        ];
        fs::write(&path, lines.iter().map(|line| serde_json::to_string(line).expect("serialize record")).collect::<Vec<_>>().join("\n")).expect("write session");
        let recorder = resume_session(&path).expect("resume");
        let active_leaf = recorder.active_leaf_id();
        recorder.record_label("a", Some("checkpoint")).expect("set label");
        let labeled = recorder.tree().expect("labeled tree").tree();
        assert_eq!(labeled[0].label.as_deref(), Some("checkpoint"));
        assert!(labeled[0].label_timestamp.is_some());
        assert_eq!(recorder.active_leaf_id(), active_leaf);
        recorder.record_label("a", None).expect("clear label");
        assert_eq!(recorder.tree().expect("cleared tree").tree()[0].label, None);
        assert_eq!(recorder.active_leaf_id(), active_leaf);
        recorder.close().expect("close recorder");

        let rows = fs::read_to_string(&path)
            .expect("read file")
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).expect("parse label row"))
            .collect::<Vec<_>>();
        assert_eq!(rows.len(), 4);
        let clear = rows.last().expect("clear row");
        assert_eq!(clear["type"], "label");
        assert!(clear.get("label").is_none());

        let resumed = resume_session(&path).expect("reopen session");
        let clone = resumed.clone();
        assert_eq!(resumed.active_leaf_id(), active_leaf);
        assert_eq!(clone.active_leaf_id(), active_leaf);
        assert_eq!(message_texts(&clone.tree().expect("clone tree").build_context(None)), vec!["root"]);
        clone.close().expect("close clone");
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn todo_snapshots_round_trip_and_follow_the_active_branch() {
        let directory = std::env::temp_dir().join(format!("pi-session-todo-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).expect("create test directory");
        let path = directory.join("session.jsonl");
        let state = crate::TodoState {
            phases: vec![crate::TodoPhase {
                name: "Build".to_owned(),
                tasks: vec![crate::TodoItem {
                    id: "task-compile".to_owned(),
                    content: "compile".to_owned(),
                    status: crate::TodoStatus::InProgress,
                    depends_on: Vec::new(),
                    ready: true,
                    blocked_by: Vec::new(),
                }, crate::TodoItem {
                    id: "task-test".to_owned(),
                    content: "test".to_owned(),
                    status: crate::TodoStatus::Pending,
                    depends_on: vec!["task-compile".to_owned()],
                    ready: false,
                    blocked_by: vec![crate::TodoBlockedReason {
                        task_id: "task-compile".to_owned(),
                        content: "compile".to_owned(),
                        status: crate::TodoStatus::InProgress,
                    }],
                }],
            }],
            storage: crate::TodoStorage::Session,
        };
        let lines = [
            json!({"type":"session", "version":CURRENT_SESSION_VERSION, "id":"todo-session", "timestamp":"2026-01-01T00:00:00.000Z", "cwd":directory}),
            json!({"type":"message", "id":"a", "parentId":null, "timestamp":"2026-01-01T00:00:01.000Z", "message":Message::user_text("root", 0)}),
            json!({"type":"todo_snapshot", "id":"todo-a", "parentId":"a", "timestamp":"2026-01-01T00:00:02.000Z", "state":state}),
        ];
        fs::write(
            &path,
            lines
                .iter()
                .map(|line| serde_json::to_string(line).expect("serialize record"))
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .expect("write session");
        let recorder = resume_session(&path).expect("resume");
        assert_eq!(recorder.latest_todo_state().expect("latest"), Some(state.clone()));
        let replacement = crate::TodoState {
            phases: Vec::new(),
            storage: crate::TodoStorage::Session,
        };
        recorder.record_todo_snapshot(&replacement).expect("record snapshot");
        assert_eq!(recorder.latest_todo_state().expect("updated"), Some(replacement));
        recorder.close().expect("close recorder");
        assert_eq!(
            load_session_tree(&path).expect("reload").latest_todo_state(),
            Some(crate::TodoState { phases: Vec::new(), storage: crate::TodoStorage::Session })
        );
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn legacy_todo_snapshot_migrates_stable_ids_on_reload() {
        let directory = std::env::temp_dir().join(format!("pi-session-todo-legacy-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).expect("create test directory");
        let path = directory.join("session.jsonl");
        let lines = [
            json!({"type":"session", "version":CURRENT_SESSION_VERSION, "id":"legacy-todo", "timestamp":"2026-01-01T00:00:00.000Z", "cwd":directory}),
            json!({"type":"message", "id":"a", "parentId":null, "timestamp":"2026-01-01T00:00:01.000Z", "message":Message::user_text("root", 0)}),
            json!({"type":"todo_snapshot", "id":"todo-a", "parentId":"a", "timestamp":"2026-01-01T00:00:02.000Z", "state":{"phases":[{"name":"Build","tasks":[{"content":"compile","status":"in_progress"},{"content":"test","status":"pending"}]}],"storage":"session"}}),
        ];
        fs::write(&path, lines.iter().map(|line| serde_json::to_string(line).expect("serialize record")).collect::<Vec<_>>().join("\n")).expect("write legacy session");
        let first = load_session_tree(&path).expect("first load").latest_todo_state().expect("todo state");
        let second = load_session_tree(&path).expect("second load").latest_todo_state().expect("todo state");
        assert_eq!(first.phases[0].tasks[0].id, second.phases[0].tasks[0].id);
        assert_eq!(first.phases[0].tasks[1].id, second.phases[0].tasks[1].id);
        assert!(first.phases[0].tasks.iter().all(|task| task.id.starts_with("task-") && task.ready));
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn branch_summary_failed_write_restores_leaf_and_pending_log() {
        let directory = std::env::temp_dir().join(format!("pi-session-summary-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).expect("create test directory");
        let occupied = directory.join("session.jsonl");
        fs::write(&occupied, b"occupied").expect("occupy path");
        let recorder = SessionRecorder { inner: Arc::new(Mutex::new(RecorderState {
            path: occupied, id: "summary-session".to_owned(), timestamp: "2026-01-01T00:00:00.000Z".to_owned(), cwd: directory.clone(), parent_session: None,
            last_id: Some("b".to_owned()), active_leaf_id: Some("b".to_owned()), revision: 0, used_ids: HashSet::from(["a".to_owned(), "b".to_owned()]),
            pending: vec![json!({"type":"session", "version":CURRENT_SESSION_VERSION, "id":"summary-session", "timestamp":"2026-01-01T00:00:00.000Z", "cwd":directory})],
            file: None, flushed: false, has_assistant: true, session_name: None, durable: false,
        })) };
        assert!(recorder.branch_with_summary(Some("a"), "cannot persist").is_err());
        assert_eq!(recorder.active_leaf_id().as_deref(), Some("b"));
        assert_eq!(recorder.inner.lock().pending.len(), 1);
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn tree_orders_multiple_roots_and_children_by_timestamp() {
        let mut late = test_entry("late", None); late.timestamp = "2026-01-01T00:00:03.000Z".to_owned();
        let mut child_late = test_entry("child-late", Some("late")); child_late.timestamp = "2026-01-01T00:00:05.000Z".to_owned();
        let mut early = test_entry("early", None); early.timestamp = "2026-01-01T00:00:01.000Z".to_owned();
        let mut child_early = test_entry("child-early", Some("late")); child_early.timestamp = "2026-01-01T00:00:04.000Z".to_owned();
        let tree = test_tree(vec![late, child_late, early, child_early], Some("child-late"));
        let roots = tree.tree();
        assert_eq!(roots.iter().map(|node| node.entry.id.as_str()).collect::<Vec<_>>(), ["early", "late"]);
        assert_eq!(roots[1].children.iter().map(|node| node.entry.id.as_str()).collect::<Vec<_>>(), ["child-early", "child-late"]);
    }


}
