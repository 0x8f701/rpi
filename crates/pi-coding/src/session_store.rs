//! Native Pi v3 append-only JSONL session storage and tree reconstruction.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, SecondsFormat, Utc};
use parking_lot::Mutex;
use pi_ai::{BranchSummaryMessage, CompactionSummaryMessage, ContentBlock, CustomMessage, CustomMessageContent, Message, Model};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::resources::agent_dir;
use crate::TodoState;

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

#[derive(Debug)]
struct RecorderState {
    path: PathBuf,
    id: String,
    timestamp: String,
    cwd: PathBuf,
    parent_session: Option<String>,
    last_id: Option<String>,
    active_leaf_id: Option<String>,
    used_ids: HashSet<String>,
    pending: Vec<Value>,
    file: Option<File>,
    flushed: bool,
    has_assistant: bool,
    session_name: Option<String>,
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

    pub fn branch(&self, entry_id: &str) -> Result<()> {
        let mut state = self.inner.lock();
        if !state.used_ids.contains(entry_id) {
            bail!("Entry not found: {entry_id}");
        }
        state.active_leaf_id = Some(entry_id.to_owned());
        Ok(())
    }

    pub fn reset_leaf(&self) {
        self.inner.lock().active_leaf_id = None;
    }

    pub fn branch_with_summary(
        &self,
        branch_from_id: Option<&str>,
        summary: &str,
    ) -> Result<String> {
        let mut state = self.inner.lock();
        if let Some(entry_id) = branch_from_id
            && !state.used_ids.contains(entry_id)
        {
            bail!("Entry not found: {entry_id}");
        }
        let previous_active_leaf_id = state.active_leaf_id.clone();
        state.active_leaf_id = branch_from_id.map(str::to_owned);
        let result = append_entry(
            &mut state,
            "branch_summary",
            json!({
                "fromId": branch_from_id.unwrap_or("root"),
                "summary": summary,
            }),
        );
        if result.is_err() {
            state.active_leaf_id = previous_active_leaf_id;
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
        let result = append_entry(
            &mut state,
            "label",
            json!({ "targetId": target_id, "label": label }),
        );
        if result.is_ok() {
            state.last_id = previous_last_id;
            state.active_leaf_id = previous_active_leaf_id;
        }
        result
    }

    pub fn fork_from(&self, entry_id: Option<&str>) {
        let mut state = self.inner.lock();
        state.last_id = entry_id.map(str::to_owned);
        state.active_leaf_id = entry_id.map(str::to_owned);
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
        let mut state = self.inner.lock();
        append_entry(
            &mut state,
            "compaction",
            json!({
                "summary": summary,
                "firstKeptEntryId": first_kept_entry_id,
                "tokensBefore": tokens_before,
                "retainedTail": retained_tail,
            }),
        )
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
        let state = self.inner.lock();
        let mut tree = if state.flushed {
            load_session_tree(&state.path)?
        } else {
            session_tree_from_values(&state.path, &state.pending)?
        };
        tree.active_leaf_id.clone_from(&state.active_leaf_id);
        Ok(tree)
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

    pub fn close(&self) -> Result<()> {
        let mut state = self.inner.lock();
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
    let id = match session_id {
        Some(id) => { validate_session_id(id)?; id.to_owned() }
        None => Uuid::now_v7().to_string(),
    };
    if list_sessions_in(&cwd, Some(&directory)).iter().any(|session| session.id == id) {
        bail!("Session already exists with id '{id}'");
    }
    let timestamp = iso_now();
    let filename = format!("{}_{}.jsonl", timestamp.replace([':', '.'], "-"), id);
    let parent_session = parent_session.map(|path| path.to_string_lossy().into_owned());
    let header = json!({
        "type": "session", "version": CURRENT_SESSION_VERSION, "id": id,
        "timestamp": timestamp, "cwd": cwd, "parentSession": parent_session,
    });
    let recorder = SessionRecorder { inner: Arc::new(Mutex::new(RecorderState {
        path: directory.join(filename), id, timestamp, cwd, parent_session,
        last_id: None, active_leaf_id: None, used_ids: HashSet::new(),
        pending: vec![header], file: None, flushed: false,
        has_assistant: false, session_name: None,
    })) };
    if let Some(model) = model { recorder.record_model_change(&model.provider, &model.id)?; }
    if let Some(level) = thinking_level.filter(|level| !level.is_empty()) {
        recorder.record_thinking_level(level)?;
    }
    Ok(recorder)
}

pub fn create_branched_session(source_path: impl AsRef<Path>, leaf_id: &str) -> Result<SessionRecorder> {
    let source_path = source_path.as_ref();
    let tree = load_session_tree(source_path)?;
    if !tree.entries.iter().any(|entry| entry.id == leaf_id) {
        bail!("Entry not found: {leaf_id}");
    }
    let branch = tree.branch(Some(leaf_id));
    let directory = source_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| default_session_dir(&tree.header.cwd));
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
        branch
            .iter()
            .map(|entry| serde_json::to_value(entry))
            .collect::<std::result::Result<Vec<_>, _>>()?,
    );
    let used_ids = branch.iter().map(|entry| entry.id.clone()).collect();
    let has_assistant = branch
        .iter()
        .any(|entry| matches!(entry.message, Some(Message::Assistant(_))));
    let session_name = branch
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
            last_id: Some(leaf_id.to_owned()),
            active_leaf_id: Some(leaf_id.to_owned()),
            used_ids,
            pending,
            file: None,
            flushed: false,
            has_assistant,
            session_name,
        })),
    };
    if has_assistant {
        persist(&mut recorder.inner.lock())?;
    }
    Ok(recorder)
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
    let id = match session_id {
        Some(id) => { validate_session_id(id)?; id.to_owned() }
        None => Uuid::now_v7().to_string(),
    };
    if list_sessions_in(&target_cwd, Some(&directory)).iter().any(|session| session.id == id) {
        bail!("Session already exists with id '{id}'");
    }
    let branch = tree.branch(tree.leaf_id.as_deref());
    let timestamp = iso_now();
    let path = directory.join(format!("{}_{}.jsonl", timestamp.replace([':', '.'], "-"), id));
    let parent_session = Some(source_path.to_string_lossy().into_owned());
    let mut pending = vec![json!({
        "type": "session", "version": CURRENT_SESSION_VERSION, "id": id,
        "timestamp": timestamp, "cwd": target_cwd, "parentSession": parent_session,
    })];
    pending.extend(branch.iter().map(|entry| serde_json::to_value(entry))
        .collect::<std::result::Result<Vec<_>, _>>()?);
    let used_ids = branch.iter().map(|entry| entry.id.clone()).collect();
    let has_assistant = branch.iter().any(|entry| matches!(entry.message, Some(Message::Assistant(_))));
    let session_name = branch.iter().rev().find(|entry| entry.entry_type == "session_info")
        .and_then(|entry| entry.name.clone());
    let leaf_id = branch.last().map(|entry| entry.id.clone());
    let recorder = SessionRecorder { inner: Arc::new(Mutex::new(RecorderState {
        path, id, timestamp, cwd: target_cwd, parent_session,
        last_id: leaf_id.clone(), active_leaf_id: leaf_id, used_ids, pending,
        file: None, flushed: false, has_assistant, session_name,
    })) };
    if has_assistant { persist(&mut recorder.inner.lock())?; }
    Ok(recorder)
}

pub fn resume_session(path: impl AsRef<Path>) -> Result<SessionRecorder> {
    let path = path.as_ref().to_path_buf();
    let tree = load_session_tree(&path)?;
    let mut file = OpenOptions::new()
        .read(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("opening session {} for append", path.display()))?;
    let requires_separator = file
        .metadata()
        .with_context(|| format!("reading session metadata {}", path.display()))?
        .len()
        > 0
        && {
            use std::io::{Read, Seek, SeekFrom};
            file.seek(SeekFrom::End(-1))
                .with_context(|| format!("seeking final session byte {}", path.display()))?;
            let mut last_byte = [0_u8; 1];
            file.read_exact(&mut last_byte)
                .with_context(|| format!("reading final session byte {}", path.display()))?;
            last_byte[0] != b'\n'
        };
    if requires_separator {
        file.write_all(b"\n")
            .with_context(|| format!("separating final session record in {}", path.display()))?;
        file.flush()
            .with_context(|| format!("flushing session separator in {}", path.display()))?;
    }
    let session_name = tree.session_name();
    let used_ids = tree.entries.iter().map(|entry| entry.id.clone()).collect();
    let active_leaf_id = tree.leaf_id.clone();
    Ok(SessionRecorder {
        inner: Arc::new(Mutex::new(RecorderState {
            path,
            id: tree.header.id,
            timestamp: tree.header.timestamp,
            cwd: tree.header.cwd,
            parent_session: tree.header.parent_session,
            last_id: tree.leaf_id,
            active_leaf_id,
            used_ids,
            pending: Vec::new(),
            file: Some(file),
            flushed: true,
            has_assistant: true,
            session_name,
        })),
    })
}

pub fn load_session_tree(path: impl AsRef<Path>) -> Result<SessionTree> {
    let path = path.as_ref();
    let file = File::open(path).with_context(|| format!("opening session {}", path.display()))?;
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
        let decoded_messages = usize::from(message.is_some())
            .saturating_add(retained_tail.len())
            .saturating_add(usize::from(matches!(
                entry_type.as_str(),
                "custom_message" | "branch_summary" | "compaction"
            )));
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
    let leaf_id = entries.last().map(|entry| entry.id.clone());
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
    let mut sessions = read_dir
        .take(MAX_SESSION_RECORDS)
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|extension| extension == "jsonl"))
        .filter_map(|entry| read_session_info(&entry.path()).ok())
        .filter(|session| !filter_cwd || absolute_path(&session.cwd) == cwd)
        .collect::<Vec<_>>();
    sessions.sort_by(|left, right| right.timestamp.cmp(&left.timestamp));
    sessions
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

fn append_entry(state: &mut RecorderState, entry_type: &str, fields: Value) -> Result<String> {
    let id = unique_entry_id(&state.used_ids);
    let previous_last_id = state.last_id.clone();
    let previous_active_leaf_id = state.active_leaf_id.clone();
    let previous_pending_len = state.pending.len();
    state.used_ids.insert(id.clone());
    let mut entry = match fields {
        Value::Object(object) => object,
        _ => Map::new(),
    };
    entry.insert("type".to_owned(), Value::String(entry_type.to_owned()));
    entry.insert("id".to_owned(), Value::String(id.clone()));
    entry.insert(
        "parentId".to_owned(),
        previous_active_leaf_id
            .as_ref()
            .map_or(Value::Null, |parent| Value::String(parent.clone())),
    );
    entry.insert("timestamp".to_owned(), Value::String(iso_now()));
    state.last_id = Some(id.clone());
    state.active_leaf_id = Some(id.clone());
    state.pending.push(Value::Object(entry));
    if let Err(error) = persist(state) {
        state.last_id = previous_last_id;
        state.active_leaf_id = previous_active_leaf_id;
        state.used_ids.remove(&id);
        state.pending.truncate(previous_pending_len);
        return Err(error);
    }
    Ok(id)
}

fn persist(state: &mut RecorderState) -> Result<()> {
    if !state.has_assistant {
        return Ok(());
    }
    if !state.flushed {
        let mut file = OpenOptions::new()
            .write(true)
            .append(true)
            .create_new(true)
            .open(&state.path)
            .with_context(|| format!("creating session {}", state.path.display()))?;
        if let Err(error) = write_records(&mut file, &state.pending) {
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
        if let Some(record) = state.pending.last() {
            write_records(file, std::slice::from_ref(record))?;
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
        self.sync_data()
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
                    timestamp: timestamp_millis(&entry.timestamp),
                }));
            }
        }
        "compaction" => {
            if let Some(summary) = entry.summary.as_deref() {
                messages.push(Message::CompactionSummary(CompactionSummaryMessage {
                    summary: summary.to_owned(),
                    tokens_before: entry.tokens_before.unwrap_or_default(),
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
        assert_eq!(list_sessions_in(cwd.path(), Some(sessions.path())).len(), 2);
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
        assert_eq!(fs::read_to_string(&path).expect("read file").lines().count(), 4);
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
                    content: "compile".to_owned(),
                    status: crate::TodoStatus::InProgress,
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
    fn branch_summary_failed_write_restores_leaf_and_pending_log() {
        let directory = std::env::temp_dir().join(format!("pi-session-summary-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).expect("create test directory");
        let occupied = directory.join("session.jsonl");
        fs::write(&occupied, b"occupied").expect("occupy path");
        let recorder = SessionRecorder { inner: Arc::new(Mutex::new(RecorderState {
            path: occupied, id: "summary-session".to_owned(), timestamp: "2026-01-01T00:00:00.000Z".to_owned(), cwd: directory.clone(), parent_session: None,
            last_id: Some("b".to_owned()), active_leaf_id: Some("b".to_owned()), used_ids: HashSet::from(["a".to_owned(), "b".to_owned()]),
            pending: vec![json!({"type":"session", "version":CURRENT_SESSION_VERSION, "id":"summary-session", "timestamp":"2026-01-01T00:00:00.000Z", "cwd":directory})],
            file: None, flushed: false, has_assistant: true, session_name: None,
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
