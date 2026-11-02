//! Native Pi v3 append-only JSONL session storage and tree reconstruction.

use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, SecondsFormat, Utc};
use parking_lot::Mutex;
use pi_ai::{
    BranchSummaryMessage, CompactionSummaryMessage, ContentBlock, CustomMessage,
    CustomMessageContent, Message, Model, Usage,
};
use pi_sandbox::AbsolutePath;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::fs::{
    CapabilityAccess, CapabilityEntry, CapabilityFile, CapabilityFs, CapabilityRootLease,
    CapabilityRootPurpose, FileIdentity, FileMode, FsError, FsMetadata, RootCreation,
};

use crate::{TodoDagExecutionStatus, TodoDagFailureReason, TodoSnapshotRecord, TodoState};

pub const CURRENT_SESSION_VERSION: u32 = 3;

const MAX_SESSION_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_SESSION_LINE_BYTES: usize = 8 * 1024 * 1024;
const MAX_SESSION_RECORDS: usize = 100_000;
const MAX_RECONSTRUCTED_MESSAGES: usize = 100_000;

/// Test-only counters proving which load path a `load_session_tree` call took:
/// the single-pass typed fast path or the lenient Value fallback. The
/// regression tests assert the fast path is engaged for writer-generated
/// files (and the fallback for unknown record shapes), so a future record
/// type that silently trips the fallback fails the fast-path invariant.
/// Thread-local so parallel test threads never contaminate each other
/// (`load_session_tree` is synchronous, so it runs on the calling test
/// thread).
#[cfg(test)]
thread_local! {
    static TYPED_LOAD_PATH_HITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static LENIENT_LOAD_PATH_HITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

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
        #[serde(
            rename = "retainedTail",
            default,
            skip_serializing_if = "Vec::is_empty"
        )]
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[serde(rename = "executionStatus")]
        execution_status: Option<TodoDagExecutionStatus>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[serde(rename = "failureReason")]
        failure_reason: Option<TodoDagFailureReason>,
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
    Checkpoint {
        id: String,
        #[serde(rename = "parentId")]
        parent_id: Option<String>,
        timestamp: String,
        name: String,
        #[serde(rename = "targetId")]
        target_id: String,
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
    #[serde(
        rename = "executionStatus",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub todo_execution_status: Option<TodoDagExecutionStatus>,
    #[serde(
        rename = "failureReason",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub todo_failure_reason: Option<TodoDagFailureReason>,
}

/// A node in the materialized session tree. Construction and serialization
/// are iterative; callers receive the forest only through crate-owned paths
/// that immediately wrap it in `SessionTreeResult` for iterative drop.
pub struct SessionTreeNode {
    pub entry: SessionEntry,
    pub children: Vec<SessionTreeNode>,
    pub label: Option<String>,
    pub label_timestamp: Option<String>,
}


impl Serialize for SessionTreeNode {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let json = self
            .to_json_string_iterative()
            .map_err(serde::ser::Error::custom)?;
        let raw =
            serde_json::value::RawValue::from_string(json).map_err(serde::ser::Error::custom)?;
        raw.serialize(serializer)
    }
}

impl SessionTreeNode {
    fn to_json_string_iterative(&self) -> serde_json::Result<String> {
        struct WriteFrame<'a> {
            node: &'a SessionTreeNode,
            next_child: usize,
        }

        fn write_prefix(json: &mut String, node: &SessionTreeNode) -> serde_json::Result<()> {
            json.push_str(r#"{"entry":"#);
            json.push_str(&serde_json::to_string(&node.entry)?);
            json.push_str(r#","children":["#);
            Ok(())
        }

        fn write_suffix(json: &mut String, node: &SessionTreeNode) -> serde_json::Result<()> {
            json.push(']');
            if let Some(label) = node.label.as_deref() {
                json.push_str(r#","label":"#);
                json.push_str(&serde_json::to_string(label)?);
            }
            if let Some(timestamp) = node.label_timestamp.as_deref() {
                json.push_str(r#","labelTimestamp":"#);
                json.push_str(&serde_json::to_string(timestamp)?);
            }
            json.push('}');
            Ok(())
        }

        let mut json = String::new();
        write_prefix(&mut json, self)?;
        let mut stack = vec![WriteFrame {
            node: self,
            next_child: 0,
        }];

        while !stack.is_empty() {
            let frame_index = stack.len() - 1;
            let frame = &mut stack[frame_index];
            if frame.next_child < frame.node.children.len() {
                if frame.next_child > 0 {
                    json.push(',');
                }
                let child = &frame.node.children[frame.next_child];
                frame.next_child += 1;
                write_prefix(&mut json, child)?;
                stack.push(WriteFrame {
                    node: child,
                    next_child: 0,
                });
                continue;
            }

            let completed = stack.swap_remove(frame_index);
            write_suffix(&mut json, completed.node)?;
        }

        Ok(json)
    }
}
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ForkMessage {
    pub entry_id: String,
    pub text: String,
}

/// Default and maximum user-candidate page for fork pickers.
pub const FORK_MESSAGES_PAGE_LIMIT: usize = 50;
/// First-line preview cap, matching the `/rewind` picker.
pub const FORK_MESSAGES_PREVIEW_CHARS: usize = 80;
/// Exact serialized data budget, reserving 16 KiB for the RPC envelope.
pub const FORK_MESSAGES_PAGE_BYTES: usize = 240 * 1024;

/// One newest-first page of fork candidates. `next_cursor` is the oldest
/// emitted entry id; the next page is strictly older. Unknown cursors fail
/// closed. `text` is a first-line preview, never the full user body.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ForkMessagePage {
    pub messages: Vec<ForkMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionEntries {
    pub entries: Vec<SessionEntry>,
    pub leaf_id: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionTreeResult {
    pub tree: Vec<SessionTreeNode>,
    pub leaf_id: Option<String>,
    pub active_leaf_id: Option<String>,
}

impl Drop for SessionTreeResult {
    fn drop(&mut self) {
        let mut pending = std::mem::take(&mut self.tree);
        while let Some(mut node) = pending.pop() {
            pending.extend(std::mem::take(&mut node.children));
        }
    }
}

impl SessionTreeResult {
    /// Serialize the tree result to a JSON string using the iterative
    /// `SessionTreeNode::Serialize` path (forward worklist + `RawValue`).
    /// This avoids the recursive `Value` tree that `to_value` builds and
    /// is safe for deep linear-chain sessions that would overflow the
    /// Tokio worker stack.
    pub fn to_json_string(&self) -> serde_json::Result<String> {
        serde_json::to_string(self)
    }
}

/// Outcome of a store-level rewind: the dropped record tail is archived to a
/// sidecar before the session file is truncated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRewindOutcome {
    /// Sidecar JSONL file holding the truncated tail records (same record
    /// serialization as the session file, header excluded).
    pub archive_path: PathBuf,
    /// Number of records dropped by the truncation.
    pub dropped_entries: usize,
    /// Number of records retained after the truncation.
    pub retained_entries: usize,
}

/// Opaque retained authority for a durable session archive and exact restore.
#[derive(Clone, Debug)]
pub struct SessionArchiveOutcome {
    session_id: String,
    original_path: PathBuf,
    archive_dir: PathBuf,
    source: CapabilityRootLease,
    archive: CapabilityRootLease,
    members: Vec<SessionArchiveMember>,
}

#[derive(Clone, Debug)]
struct SessionArchiveMember {
    original_name: OsString,
    archive_name: OsString,
    archive_metadata: FsMetadata,
}

impl SessionArchiveOutcome {
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    #[must_use]
    pub fn original_path(&self) -> &Path {
        &self.original_path
    }

    #[must_use]
    pub fn archive_dir(&self) -> &Path {
        &self.archive_dir
    }
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
    pub skipped_idless: usize,
    pub undecodable_messages: usize,
    by_id: HashMap<String, usize>,
    children_by_parent: HashMap<Option<String>, Vec<usize>>,
    labels: HashMap<String, ResolvedLabel>,
}

impl SessionTree {
    #[must_use]
    pub fn has_id(&self, id: &str) -> bool {
        self.by_id.contains_key(id)
    }

    #[must_use]
    pub fn has_loader_drops(&self) -> bool {
        self.skipped_idless > 0 || self.undecodable_messages > 0
    }

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

    /// Newest-first page of forkable user messages. `cursor` is the oldest
    /// entry id from the previous page; unknown or non-candidate cursors fail
    /// closed. `text` is a first-line preview (max 80 chars), never the full
    /// user body.
    pub fn fork_messages_page(
        &self,
        cursor: Option<&str>,
        limit: Option<usize>,
    ) -> Result<ForkMessagePage> {
        let limit = match limit {
            None => FORK_MESSAGES_PAGE_LIMIT,
            Some(0) => bail!("limit must be greater than 0"),
            Some(n) => n.min(FORK_MESSAGES_PAGE_LIMIT),
        };
        let start_exclusive = match cursor {
            None => self.entries.len(),
            Some(cursor) => {
                if cursor.is_empty() {
                    bail!("invalid fork cursor");
                }
                let index = self
                    .by_id
                    .get(cursor)
                    .copied()
                    .ok_or_else(|| anyhow!("invalid fork cursor"))?;
                if !self.entries[index]
                    .message
                    .as_ref()
                    .is_some_and(is_fork_candidate)
                {
                    bail!("invalid fork cursor");
                }
                index
            }
        };

        let mut messages = Vec::new();
        let mut more = false;
        for index in (0..start_exclusive).rev() {
            let entry = &self.entries[index];
            let Some(text) = entry.message.as_ref().and_then(user_fork_preview) else {
                continue;
            };
            if messages.len() >= limit {
                more = true;
                break;
            }
            messages.push(ForkMessage {
                entry_id: entry.id.clone(),
                text,
            });
        }

        let total = self
            .entries
            .iter()
            .filter(|entry| entry.message.as_ref().is_some_and(is_fork_candidate))
            .count();
        let mut page = ForkMessagePage {
            next_cursor: more
                .then(|| messages.last().map(|message| message.entry_id.clone()))
                .flatten(),
            truncated: more,
            total: Some(total),
            messages,
        };
        while serde_json::to_vec(&page)?.len() > FORK_MESSAGES_PAGE_BYTES {
            page.messages.pop();
            let Some(last) = page.messages.last() else {
                bail!("fork candidate cannot fit within the page byte limit");
            };
            page.truncated = true;
            page.next_cursor = Some(last.entry_id.clone());
        }
        Ok(page)
    }

    #[must_use]
    pub(crate) fn tree(&self) -> SessionTreeResult {
        let roots = self
            .children_by_parent
            .get(&None)
            .into_iter()
            .flatten()
            .copied();
        let mut forest = Vec::new();
        for root_index in roots {
            forest.push(self.build_node_iterative(root_index));
        }
        SessionTreeResult {
            tree: forest,
            leaf_id: self.leaf_id.clone(),
            active_leaf_id: self.active_leaf_id.clone(),
        }
    }

    fn build_node_iterative(&self, root: usize) -> SessionTreeNode {
        struct Frame {
            child_indices: Vec<usize>,
            child_pos: usize,
            node: SessionTreeNode,
            index: usize,
        }

        let root_label = self.labels.get(&self.entries[root].id);
        let root_children = self
            .children_by_parent
            .get(&Some(self.entries[root].id.clone()))
            .cloned()
            .unwrap_or_default();
        let mut visiting = HashSet::from([root]);
        let mut stack = vec![Frame {
            child_indices: root_children,
            child_pos: 0,
            node: SessionTreeNode {
                entry: self.entries[root].clone(),
                children: Vec::new(),
                label: root_label.map(|label| label.label.clone()),
                label_timestamp: root_label.map(|label| label.timestamp.clone()),
            },
            index: root,
        }];

        loop {
            let frame_index = stack.len() - 1;
            let frame = &mut stack[frame_index];
            if frame.child_pos < frame.child_indices.len() {
                let child_index = frame.child_indices[frame.child_pos];
                frame.child_pos += 1;
                if !visiting.insert(child_index) {
                    continue;
                }
                let child_children = self
                    .children_by_parent
                    .get(&Some(self.entries[child_index].id.clone()))
                    .cloned()
                    .unwrap_or_default();
                let child_label = self.labels.get(&self.entries[child_index].id);
                stack.push(Frame {
                    child_indices: child_children,
                    child_pos: 0,
                    node: SessionTreeNode {
                        entry: self.entries[child_index].clone(),
                        children: Vec::new(),
                        label: child_label.map(|label| label.label.clone()),
                        label_timestamp: child_label.map(|label| label.timestamp.clone()),
                    },
                    index: child_index,
                });
                continue;
            }

            let completed = stack.swap_remove(frame_index);
            visiting.remove(&completed.index);
            if stack.is_empty() {
                return completed.node;
            }
            let parent_index = stack.len() - 1;
            stack[parent_index].node.children.push(completed.node);
        }
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
        self.latest_todo_snapshot().map(|snapshot| snapshot.state)
    }

    #[must_use]
    pub fn latest_todo_snapshot(&self) -> Option<TodoSnapshotRecord> {
        self.branch(None).into_iter().rev().find_map(|entry| {
            entry.todo_state.clone().map(|state| TodoSnapshotRecord {
                state,
                execution_status: entry.todo_execution_status,
                failure_reason: entry.todo_failure_reason.clone(),
            })
        })
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
                    if let Some(Message::Assistant(message)) = &entry.message
                        && !message.provider.is_empty()
                        && !message.model.is_empty()
                    {
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
                context
                    .messages
                    .push(Message::CompactionSummary(CompactionSummaryMessage {
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

/// Recorder snapshot whose branch, identity, and authority were captured under
/// the same lock. Consumers can durably fork this exact parent revision without
/// reopening the parent path or observing a later active leaf.
pub struct ParentForkPoint {
    pub parent_session_id: String,
    pub parent_session_path: PathBuf,
    pub parent_file_identity: Option<crate::SessionFileIdentity>,
    pub parent_active_leaf_id: Option<String>,
    pub parent_revision: u64,
    pub tree: SessionTree,
    raw_records: Vec<Value>,
    pub lease: CapabilityRootLease,
    pub relative: PathBuf,
}

fn is_fork_candidate(message: &Message) -> bool {
    user_fork_preview(message).is_some()
}

/// First-line preview of a nonempty user message, capped at
/// [`FORK_MESSAGES_PREVIEW_CHARS`]. Assistant/tool/system messages are not
/// fork candidates. Empty-text users stay unlistable.
fn user_fork_preview(message: &Message) -> Option<String> {
    let Message::User(user) = message else {
        return None;
    };
    let mut first_line = String::new();
    for block in &user.content {
        let ContentBlock::Text { text, .. } = block else {
            continue;
        };
        for line in text.lines() {
            if first_line.is_empty() {
                let piece = line.trim_start();
                if piece.is_empty() {
                    continue;
                }
                first_line.push_str(piece);
            }
            break;
        }
        if !first_line.is_empty() {
            break;
        }
    }
    let first_line = first_line.trim();
    if first_line.is_empty() {
        return None;
    }
    let capped = first_line
        .chars()
        .take(FORK_MESSAGES_PREVIEW_CHARS)
        .collect::<String>();
    (!capped.is_empty()).then_some(capped)
}

/// Durable child journal produced by a create-time fork.
///
/// `editor_text` is the selected user prompt concatenated from its text
/// blocks, including empty strings. Image-only users yield `""`. The selected
/// user is never written into `recorder`.
#[derive(Debug)]
pub struct MaterializedFork {
    pub recorder: SessionRecorder,
    pub editor_text: String,
}

/// Captured source tree plus retained lineage needed to materialize a fork
/// without reopening the parent path.
pub struct ForkSourceSnapshot {
    pub path: PathBuf,
    pub tree: SessionTree,
    raw_records: Vec<Value>,
}

impl From<ParentForkPoint> for ForkSourceSnapshot {
    fn from(fork: ParentForkPoint) -> Self {
        Self {
            path: fork.parent_session_path,
            tree: fork.tree,
            raw_records: fork.raw_records,
        }
    }
}

impl From<PreparedSessionResume> for ForkSourceSnapshot {
    fn from(prepared: PreparedSessionResume) -> Self {
        Self {
            path: prepared.path,
            tree: prepared.tree,
            raw_records: prepared.raw_records,
        }
    }
}

/// Concatenate a user message's text blocks, including empty ones.
/// Image-only users yield an empty string. Non-user messages return `None`.
#[must_use]
pub fn fork_selected_editor_text(message: &Message) -> Option<String> {
    let Message::User(user) = message else {
        return None;
    };
    Some(
        user.content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect(),
    )
}


#[derive(Debug)]
struct RecorderState {
    path: PathBuf,
    id: String,
    file_identity: Option<crate::SessionFileIdentity>,
    timestamp: String,
    cwd: PathBuf,
    parent_session: Option<String>,
    last_id: Option<String>,
    active_leaf_id: Option<String>,
    revision: u64,
    used_ids: HashSet<String>,
    pending: Vec<Value>,
    requires_separator: bool,
    file: Option<File>,
    lease: Option<crate::fs::CapabilityRootLease>,
    flushed: bool,
    has_assistant: bool,
    session_name: Option<String>,
    durable: bool,
}

#[derive(Debug)]
pub struct PreparedSessionResume {
    path: PathBuf,
    tree: SessionTree,
    raw_records: Vec<Value>,
    file: File,
    identity: crate::SessionFileIdentity,
    lease: crate::fs::CapabilityRootLease,
    relative: PathBuf,
    requires_separator: bool,
}

impl PreparedSessionResume {
    pub fn prepare_path(path: impl AsRef<Path>) -> Result<Self> {
        let path = absolute_path(path.as_ref());
        let capabilities =
            CapabilityFs::new().context("initializing session filesystem capability")?;
        let absolute = AbsolutePath::parse(path.clone())
            .map_err(|_| anyhow!("invalid session path {}", path.display()))?;
        let (parent, name) = capabilities.open_exact_parent(
            CapabilityRootPurpose::SessionStore,
            absolute,
            CapabilityAccess::PrivateReadWrite,
            RootCreation::Existing,
        )?;
        Self::from_capability_file(parent.open_read_write_file(Path::new(&name))?)
    }

    pub(crate) fn prepare_under_lease(
        child_root: &CapabilityRootLease,
        relative: &Path,
    ) -> Result<Self> {
        Self::from_capability_file(child_root.open_read_write_file(relative)?)
    }

    pub(crate) fn prepare_under_root(root: &Path, path: &Path) -> Result<Self> {
        let root = absolute_path(root);
        let path = absolute_path(path);
        let relative = path.strip_prefix(&root).map_err(|_| {
            anyhow!(
                "session {} is outside root {}",
                path.display(),
                root.display()
            )
        })?;
        let parent_relative = relative.parent().unwrap_or_else(|| Path::new(""));
        let name = relative
            .file_name()
            .ok_or_else(|| anyhow!("session path has no file name"))?;
        let capabilities =
            CapabilityFs::new().context("initializing session filesystem capability")?;
        let root_lease = capabilities.open_root(
            CapabilityRootPurpose::SessionStore,
            AbsolutePath::parse(root).map_err(|_| anyhow!("invalid session root"))?,
            CapabilityAccess::PrivateReadWrite,
            RootCreation::Existing,
        )?;
        let parent = root_lease
            .lease_directory(
                parent_relative,
                CapabilityAccess::PrivateReadWrite,
                RootCreation::Existing,
            )
            .context("retaining session file parent")?;
        Self::from_capability_file(parent.open_read_write_file(Path::new(name))?)
    }

    fn from_capability_file(opened: crate::fs::CapabilityFile) -> Result<Self> {
        let path = opened.display_path();
        let identity = opened.public_identity();
        let (lease, relative) = opened.authority();
        let mut file = opened
            .try_clone_std()
            .with_context(|| format!("cloning retained session handle {}", path.display()))?;
        let values = read_session_values_from_file(
            file.try_clone()
                .with_context(|| format!("cloning retained session handle {}", path.display()))?,
            &path,
        )?;
        let tree = session_tree_from_values(&path, &values)?;
        let raw_records = values.into_iter().skip(1).collect();
        let requires_separator = session_requires_separator(&mut file, &path)?;
        Ok(Self {
            path,
            tree,
            raw_records,
            file,
            identity,
            lease,
            relative,
            requires_separator,
        })
    }

    #[must_use]
    pub const fn identity(&self) -> crate::SessionFileIdentity {
        self.identity
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn file_authority(&self) -> (crate::fs::CapabilityRootLease, PathBuf) {
        (self.lease.clone(), self.relative.clone())
    }

    #[must_use]
    pub fn tree(&self) -> &SessionTree {
        &self.tree
    }

    #[must_use]
    pub fn target_cwd(&self) -> &Path {
        &self.tree.header.cwd
    }
    pub(crate) fn into_read_file(self) -> File {
        self.file
    }

    #[must_use]
    pub fn build_context(&self) -> BranchContext {
        self.tree.build_context(None)
    }

    pub fn into_recorder(self) -> Result<SessionRecorder> {
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
                file_identity: Some(self.identity),
                id: self.tree.header.id,
                timestamp: self.tree.header.timestamp,
                cwd: self.tree.header.cwd,
                parent_session: self.tree.header.parent_session,
                last_id: self.tree.leaf_id,
                active_leaf_id,
                revision: 0,
                used_ids,
                pending: Vec::new(),
                requires_separator: self.requires_separator,
                file: Some(self.file),
                lease: Some(self.lease),
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
    pub fn file_identity(&self) -> Option<crate::SessionFileIdentity> {
        self.inner.lock().file_identity
    }

    pub(crate) fn file_authority(&self) -> Option<(crate::fs::CapabilityRootLease, PathBuf)> {
        let state = self.inner.lock();
        let lease = state.lease.as_ref()?.clone();
        let relative = state
            .path
            .strip_prefix(lease.display_root())
            .ok()?
            .to_path_buf();
        Some((lease, relative))
    }

    /// Materialize and validate the recorder's retained append writer before
    /// the recorder becomes session-visible. Once activated, every append is
    /// flushed and fsynced through this retained descriptor.
    pub fn activate_writer(
        &self,
    ) -> Result<(String, PathBuf, crate::SessionFileIdentity)> {
        let mut state = self.inner.lock();
        if !state.flushed {
            persist_durable(&mut state)?;
        }
        state.durable = true;
        validate_append_writer(&mut state)
    }

    /// Revalidate the already-activated retained descriptor, capability root,
    /// path identity, and append position.
    pub fn writer_info(&self) -> Result<(String, PathBuf, crate::SessionFileIdentity)> {
        validate_append_writer(&mut self.inner.lock())
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

    /// Mark the current position as a named rewind target.
    ///
    /// Appends a `checkpoint` record pointing at the current leaf and then
    /// restores the leaf pointers, so the marker (like a label) is a side
    /// record that never joins the linear record chain and never appears in
    /// the reconstructed transcript. A later `/rewind <name>` rolls the
    /// session back to the marked entry. Recording a checkpoint with an
    /// existing name shadows the older marker (the newest wins on resolve).
    pub fn record_checkpoint(&self, name: &str) -> Result<String> {
        let normalized = normalize_checkpoint_name(name)?;
        let mut state = self.inner.lock();
        let target_id = state
            .last_id
            .clone()
            .ok_or_else(|| anyhow!("cannot checkpoint an empty session"))?;
        let previous_last_id = state.last_id.clone();
        let previous_active_leaf_id = state.active_leaf_id.clone();
        let mut fields = Map::new();
        fields.insert("name".to_owned(), Value::String(normalized));
        fields.insert("targetId".to_owned(), Value::String(target_id));
        let result = append_entry(&mut state, "checkpoint", Value::Object(fields));
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

    pub fn record_custom_entry(&self, custom_type: &str, data: Option<Value>) -> Result<String> {
        let mut fields = Map::new();
        fields.insert(
            "customType".to_owned(),
            Value::String(custom_type.to_owned()),
        );
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
        fields.insert(
            "customType".to_owned(),
            Value::String(custom_type.to_owned()),
        );
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

    /// Record the complete Todo state and make it crash-durable before returning.
    ///
    /// Todo mutations can happen before the first assistant message, while the
    /// ordinary transcript writer is still lazy. A snapshot must not remain in
    /// that in-memory queue: after a successful Todo operation, resume must see
    /// the same state even if the process is terminated without a clean close.
    pub fn record_todo_snapshot(&self, state: &TodoState) -> Result<String> {
        self.record_todo_snapshot_record(&TodoSnapshotRecord::from_state(state.clone()))
    }

    pub fn record_todo_snapshot_record(&self, snapshot: &TodoSnapshotRecord) -> Result<String> {
        let mut recorder = self.inner.lock();
        let mut payload = json!({ "state": snapshot.state });
        if let Some(status) = snapshot.execution_status {
            payload["executionStatus"] = serde_json::to_value(status)?;
        }
        if let Some(reason) = &snapshot.failure_reason {
            payload["failureReason"] = serde_json::to_value(reason)?;
        }
        append_entry_durable(&mut recorder, "todo_snapshot", payload)
    }

    pub fn latest_todo_state(&self) -> Result<Option<TodoState>> {
        Ok(self.tree()?.latest_todo_state())
    }

    pub fn latest_todo_snapshot(&self) -> Result<Option<TodoSnapshotRecord>> {
        Ok(self.tree()?.latest_todo_snapshot())
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
        fields.insert(
            "retainedTail".to_owned(),
            serde_json::to_value(retained_tail)?,
        );
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

    /// Captures every parent attribute needed by a durable fork while the
    /// recorder mutation lock prevents branch navigation or appends.
    pub fn capture_fork_point(&self) -> Result<ParentForkPoint> {
        let state = self.inner.lock();
        // A live fork is authoritative only while the recorder still owns its
        // retained descriptor. Falling back to `state.path` here would reopen
        // a mutable pathname and could materialize a replacement inode rather
        // than the runtime's captured journal.
        if state.flushed && state.file.is_none() {
            bail!("session append writer is unavailable for live fork capture");
        }
        let (tree, raw_records) = fork_tree_and_raw_records_from_state(&state)?;
        let lease = state
            .lease
            .as_ref()
            .cloned()
            .ok_or_else(|| anyhow!("session file authority is unavailable"))?;
        let relative = state
            .path
            .strip_prefix(lease.display_root())
            .with_context(|| {
                format!(
                    "session {} is outside retained root {}",
                    state.path.display(),
                    lease.display_root().display()
                )
            })?
            .to_path_buf();
        Ok(ParentForkPoint {
            parent_session_id: state.id.clone(),
            parent_session_path: state.path.clone(),
            parent_file_identity: state.file_identity,
            parent_active_leaf_id: state.active_leaf_id.clone(),
            parent_revision: state.revision,
            tree,
            raw_records,
            lease,
            relative,
        })
    }

    /// Roll the session file back to the first `keep` records.
    ///
    /// Records at index `keep` and beyond are dropped: the tail is first
    /// archived verbatim to a `.rewind-<timestamp>.jsonl` sidecar next to the
    /// session file (safety net — the truncated records are recoverable), then
    /// the file is truncated and synced, and the recorder's in-memory leaf,
    /// id set, assistant flag, and session name are rebuilt from the retained
    /// records. The recorder remains open and appendable at the new end.
    ///
    /// Safety bounds are enforced here too (not only by callers): rewinding
    /// past the first record (`keep == 0`) or to the end (`keep >= total`)
    /// is refused so a rewind can never leave an empty or unchanged journal.
    pub fn rewind_to(&self, keep: usize) -> Result<SessionRewindOutcome> {
        let mut state = self.inner.lock();
        // Flush pending records so the on-disk file holds the full record set
        // before the cut is located and the tail is archived.
        let previous_has_assistant = state.has_assistant;
        state.has_assistant = true;
        let flush = persist(&mut state);
        state.has_assistant = previous_has_assistant;
        flush?;
        let tree = session_tree_from_state(&state)?;
        let total = tree.entries.len();
        anyhow::ensure!(
            keep >= 1,
            "rewind refused: cannot rewind past the first entry (entry index 0 is the earliest record)"
        );
        anyhow::ensure!(
            keep < total,
            "nothing to rewind: the session has {total} record(s); entry index {keep} is at or beyond the end"
        );
        // Locate the kept record through a cloned retained descriptor. The
        // append-mode writer may sit at EOF, so reset the clone before reading.
        let cut_id = tree.entries[keep].id.clone();
        let session_path = state.path.clone();
        let file = state
            .file
            .as_ref()
            .ok_or_else(|| anyhow!("session file is unavailable for rewind"))?;
        let mut reader = file
            .try_clone()
            .context("cloning session handle for rewind scan")?;
        reader.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        reader
            .take(MAX_SESSION_FILE_BYTES + 1)
            .read_to_end(&mut bytes)
            .with_context(|| {
                format!("reading session {} for rewind scan", session_path.display())
            })?;
        if bytes.len() as u64 > MAX_SESSION_FILE_BYTES {
            bail!(
                "session {} exceeds the 64 MiB file safety limit",
                session_path.display()
            );
        }
        let mut offset = 0u64;
        let mut cut_offset = None;
        for line in bytes.split(|byte| *byte == b'\n') {
            if let Ok(value) = serde_json::from_slice::<Value>(trim_ascii_whitespace(line))
                && value.get("id").and_then(Value::as_str) == Some(cut_id.as_str())
            {
                cut_offset = Some(offset);
                break;
            }
            offset = offset.saturating_add((line.len() + 1) as u64);
        }
        let cut_offset = cut_offset.ok_or_else(|| {
            anyhow!("rewind cut record {cut_id} was not found in the session file")
        })?;
        // Archive the dropped tail verbatim before truncating.
        let tail = bytes[(cut_offset as usize)..].to_vec();
        let archive_path = archive_rewind_tail(&mut state, &tail)?;
        // Truncate and sync; the file was opened append-mode, so subsequent
        // record writes land at the new end automatically.
        let file = state
            .file
            .as_mut()
            .ok_or_else(|| anyhow!("session file is unavailable for rewind"))?;
        file.set_len(cut_offset)
            .with_context(|| format!("truncating session at rewind cut {cut_offset}"))?;
        file.sync_all()
            .with_context(|| format!("syncing truncated session {}", session_path.display()))?;
        // Rebuild the in-memory recorder state from the retained records.
        let kept = &tree.entries[..keep];
        state.pending.clear();
        state.requires_separator = false;
        state.last_id = kept.last().map(|entry| entry.id.clone());
        state.active_leaf_id = kept.last().map(|entry| entry.id.clone());
        state.used_ids = kept.iter().map(|entry| entry.id.clone()).collect();
        state.revision = state.revision.saturating_add(1);
        state.has_assistant = kept
            .iter()
            .any(|entry| matches!(entry.message, Some(Message::Assistant(_))));
        state.session_name = kept
            .iter()
            .rev()
            .find(|entry| entry.entry_type == "session_info")
            .and_then(|entry| entry.name.as_deref())
            .and_then(normalize_session_name);
        state.flushed = true;
        Ok(SessionRewindOutcome {
            archive_path,
            dropped_entries: total - keep,
            retained_entries: keep,
        })
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

    /// Remove this recorder's just-created journal through its retained
    /// capability, but only while the path still names the exact file identity
    /// created by this recorder. Used to roll back an unpublished runtime
    /// candidate after a later initialization step fails.
    pub fn discard_persisted(&self) -> Result<()> {
        let mut state = self.inner.lock();
        let identity = state
            .file_identity
            .ok_or_else(|| anyhow!("session file identity is unavailable"))?;
        let lease = state
            .lease
            .as_ref()
            .cloned()
            .ok_or_else(|| anyhow!("session directory capability is unavailable"))?;
        let relative = state
            .path
            .strip_prefix(lease.display_root())
            .context("session path is outside retained session root")?
            .to_path_buf();
        let opened = lease
            .open_file(&relative)
            .context("opening unpublished session for rollback")?;
        if opened.public_identity() != identity {
            bail!("unpublished session identity changed before rollback");
        }
        lease
            .remove_file_expected(&relative, opened.identity())
            .context("removing unpublished session")?;
        lease
            .sync_directory(Path::new(""))
            .context("syncing unpublished-session rollback")?;
        state.file = None;
        state.file_identity = None;
        state.flushed = false;
        state.requires_separator = false;
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

/// Resolve the `.rpi` base used for the native session store.
///
/// Reuses the shared agent/config precedence `PI_CODING_AGENT_DIR` >
/// `RPI_HOME` > `HOME/.rpi`, then relocates under the active config profile
/// exactly once so the session store, resource loader, and catalog cannot
/// resolve different agent roots.
fn session_store_agent_base() -> PathBuf {
    crate::resources::apply_profile(crate::resources::agent_dir_base())
}

pub fn default_session_dir(cwd: impl AsRef<Path>) -> PathBuf {
    let cwd = absolute_path(cwd.as_ref());
    session_store_agent_base()
        .join("sessions")
        .join(format!("--{}--", encode_cwd_safe_path(&cwd)))
}

/// Default age after which an untouched native session file is pruned at
/// startup. Overridable per-install via the `sessionTtlDays` setting
/// (`Settings::session_ttl_days`); a value there replaces this default.
pub const DEFAULT_SESSION_TTL_DAYS: u64 = 30;

/// Sessions modified within this window are treated as possibly active and are
/// never pruned, regardless of TTL. The native store has no per-session lock
/// or `.active` marker, so recency is the conservative active-session guard.
pub const SESSION_ACTIVE_GRACE: Duration = Duration::from_secs(60 * 60);

/// Earliest plausible last-modified time for a native session file
/// (2000-01-01T00:00:00Z). The native v3 store did not exist before the
/// 2020s, so an mtime older than this is a planted fixture, a restored
/// archive, or clock-skewed data — never a live session. Such files are
/// skipped exactly like future mtimes: the implausible is never pruned.
const PRUNE_MTIME_FLOOR_SECS: u64 = 946_684_800;

/// Absolute root of the native sessions tree (`<agent base>/sessions`), the
/// parent of every per-cwd `--<encoded-cwd>--` directory and the `children/`
/// subtree holding durable child sessions. Matches the catalog's native root
/// (`SessionCatalog::root_for(SessionSourceKind::NativePi)`).
#[must_use]
pub fn native_sessions_root() -> PathBuf {
    session_store_agent_base().join("sessions")
}

fn new_session_path(directory: &Path, timestamp: &str, id: &str, has_explicit_id: bool) -> PathBuf {
    if has_explicit_id {
        return directory.join(format!("{id}.jsonl"));
    }
    directory.join(format!(
        "{}_{}.jsonl",
        timestamp.replace([':', '.'], "-"),
        id
    ))
}

/// Authoritative runtime provenance in a session header's `sessionKind`
/// field: absent means [`SessionKind::User`]; workflow-internal kinds and an
/// unrecognized value ([`SessionKind::Unknown`]) are never user-switchable
/// catalog rows. Classification never uses cwd, path, or file name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionKind {
    User,
    WorkflowSupervisor,
    WorkflowPlanner,
    WorkflowWorker,
    WorkflowWorktree,
    Disposable,
    Unknown,
}

impl SessionKind {
    pub const ALL: [Self; 7] = [
        Self::User,
        Self::WorkflowSupervisor,
        Self::WorkflowPlanner,
        Self::WorkflowWorker,
        Self::WorkflowWorktree,
        Self::Disposable,
        Self::Unknown,
    ];

    #[must_use]
    pub const fn header_value(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::WorkflowSupervisor => "workflowSupervisor",
            Self::WorkflowPlanner => "workflowPlanner",
            Self::WorkflowWorker => "workflowWorker",
            Self::WorkflowWorktree => "workflowWorktree",
            Self::Disposable => "disposable",
            Self::Unknown => "unknown",
        }
    }

    #[must_use]
    pub fn from_header_value(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.header_value() == value)
    }

    #[must_use]
    pub const fn is_workflow_internal(self) -> bool {
        !matches!(self, Self::User)
    }
}

pub fn start_session_in(
    cwd: impl AsRef<Path>,
    model: Option<&Model>,
    thinking_level: Option<&str>,
    session_dir: Option<&Path>,
    session_id: Option<&str>,
    parent_session: Option<&Path>,
) -> Result<SessionRecorder> {
    start_session_in_with_kind(
        cwd,
        model,
        thinking_level,
        session_dir,
        session_id,
        parent_session,
        None,
    )
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

pub fn start_session_in_with_kind(
    cwd: impl AsRef<Path>,
    model: Option<&Model>,
    thinking_level: Option<&str>,
    session_dir: Option<&Path>,
    session_id: Option<&str>,
    parent_session: Option<&Path>,
    session_kind: Option<SessionKind>,
) -> Result<SessionRecorder> {
    let cwd = absolute_path(cwd.as_ref());
    let directory = session_dir.map_or_else(|| default_session_dir(&cwd), absolute_path);
    let lease = open_session_store_lease(&directory, RootCreation::CreateDirectory)?;
    let has_explicit_id = session_id.is_some();
    let id = match session_id {
        Some(id) => {
            validate_session_id(id)?;
            id.to_owned()
        }
        None => Uuid::now_v7().to_string(),
    };
    if list_sessions_in(&cwd, Some(&directory))
        .iter()
        .any(|session| session.id == id)
    {
        bail!("Session already exists with id '{id}'");
    }
    let timestamp = iso_now();
    let path = new_session_path(&directory, &timestamp, &id, has_explicit_id);
    let parent_session = parent_session.map(|path| path.to_string_lossy().into_owned());
    let mut header = json!({
        "type": "session", "version": CURRENT_SESSION_VERSION, "id": id,
        "timestamp": timestamp, "cwd": cwd, "parentSession": parent_session,
    });
    if let Some(kind) = session_kind.filter(|kind| kind.is_workflow_internal()) {
        header["sessionKind"] = json!(kind.header_value());
    }
    let recorder = SessionRecorder {
        inner: Arc::new(Mutex::new(RecorderState {
            path,
            id,
            timestamp,
            cwd,
            parent_session,
            file_identity: None,
            last_id: None,
            active_leaf_id: None,
            revision: 0,
            used_ids: HashSet::new(),
            pending: vec![header],
            requires_separator: false,
            file: None,
            flushed: false,
            lease: Some(lease),
            has_assistant: false,
            session_name: None,
            durable: false,
        })),
    };
    if has_explicit_id {
        recorder
            .persist_now()
            .context("reserving explicit session id")?;
    }
    if let Some(model) = model {
        recorder.record_model_change(&model.provider, &model.id)?;
    }
    if let Some(level) = thinking_level.filter(|level| !level.is_empty()) {
        recorder.record_thinking_level(level)?;
    }
    Ok(recorder)
}

pub fn create_branched_session(
    source_path: impl AsRef<Path>,
    leaf_id: &str,
) -> Result<SessionRecorder> {
    create_branched_session_in(source_path, leaf_id, None)
}

pub(crate) fn create_branched_session_from_recorder(
    source: &SessionRecorder,
    leaf_id: &str,
    session_dir: Option<&Path>,
) -> Result<SessionRecorder> {
    let tree = source.tree()?;
    create_branched_session_from_tree(&source.path(), tree, leaf_id, session_dir)
}

pub fn create_branched_session_in(
    source_path: impl AsRef<Path>,
    leaf_id: &str,
    session_dir: Option<&Path>,
) -> Result<SessionRecorder> {
    let source_path = source_path.as_ref();
    let tree = load_session_tree(source_path)?;
    create_branched_session_from_tree(source_path, tree, leaf_id, session_dir)
}

pub fn create_branched_session_from_prepared(
    prepared: PreparedSessionResume,
    leaf_id: &str,
    session_dir: Option<&Path>,
) -> Result<SessionRecorder> {
    let source_path = prepared.path;
    let tree = prepared.tree;
    create_forked_session_from_tree(
        &source_path,
        tree,
        Some(prepared.raw_records),
        Some(leaf_id),
        session_dir,
        false,
    )
}

/// Materialize a standalone child journal at a selected user from one
/// captured source tree.
///
/// The retained cut is `selected.parent_id`. A `None` parent produces a
/// header-only child (empty ancestry, no synthetic model/thinking records).
/// Nested forks copy every ordinary ancestry record through that parent,
/// excluding the selected user and its descendants. Labels are recreated
/// from the final resolved map. The child is create-new + fsync'd before
/// this function returns, so publication never depends on the parent file.
pub fn materialize_fork_at_user(
    source: ForkSourceSnapshot,
    entry_id: &str,
    session_dir: Option<&Path>,
) -> Result<MaterializedFork> {
    let selected = source
        .tree
        .entries
        .iter()
        .find(|entry| entry.id == entry_id)
        .ok_or_else(|| anyhow!("Entry not found: {entry_id}"))?;
    let editor_text = fork_selected_editor_text(
        selected
            .message
            .as_ref()
            .ok_or_else(|| anyhow!("Invalid entry ID for forking"))?,
    )
    .ok_or_else(|| anyhow!("Invalid entry ID for forking"))?;
    let cut_point = selected.parent_id.clone();
    let recorder = create_forked_session_from_tree(
        &source.path,
        source.tree,
        Some(source.raw_records),
        cut_point.as_deref(),
        session_dir,
        true,
    )?;

    Ok(MaterializedFork {
        recorder,
        editor_text,
    })
}

pub fn fork_prepared_session_at_user(
    prepared: PreparedSessionResume,
    entry_id: &str,
    session_dir: Option<&Path>,
) -> Result<SessionRecorder> {
    Ok(materialize_fork_at_user(prepared.into(), entry_id, session_dir)?.recorder)
}

pub fn fork_live_session_at_user(
    source: &SessionRecorder,
    entry_id: &str,
    session_dir: Option<&Path>,
) -> Result<MaterializedFork> {
    let captured = source.capture_fork_point()?;
    materialize_fork_at_user(captured.into(), entry_id, session_dir)
}


struct RetainedBranch {
    rows: Vec<Value>,
    used_ids: HashSet<String>,
    leaf_id: Option<String>,
    has_assistant: bool,
    session_name: Option<String>,
    labels: Vec<Value>,
}
fn retain_branch(
    tree: &SessionTree,
    raw_records: &[Value],
    leaf_id: Option<&str>,
) -> Result<RetainedBranch> {
    if let Some(leaf_id) = leaf_id
        && !tree.entries.iter().any(|entry| entry.id == leaf_id)
    {
        bail!("Entry not found: {leaf_id}");
    }

    let ordinary_entries = tree
        .entries
        .iter()
        .filter(|entry| entry.entry_type != "label" && entry.entry_type != "checkpoint")
        .map(|entry| (entry.id.as_str(), entry))
        .collect::<HashMap<_, _>>();
    let mut raw_by_id = HashMap::with_capacity(ordinary_entries.len());
    for raw in raw_records {
        let object = raw
            .as_object()
            .ok_or_else(|| anyhow!("fork source contains a non-object record"))?;
        let entry_type = object
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("fork source record is missing type"))?;
        if entry_type == "label" || entry_type == "checkpoint" {
            continue;
        }
        let id = object
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| anyhow!("fork source {entry_type} record is missing id"))?;
        let typed = ordinary_entries
            .get(id)
            .ok_or_else(|| anyhow!("fork source raw record {id} has no typed entry"))?;
        if typed.entry_type != entry_type {
            bail!(
                "fork source record {id} type mismatch: raw {entry_type}, typed {}",
                typed.entry_type
            );
        }
        if raw_by_id.insert(id, raw).is_some() {
            bail!("fork source contains duplicate raw record id {id}");
        }
    }
    if raw_by_id.len() != ordinary_entries.len() {
        let missing = ordinary_entries
            .keys()
            .find(|id| !raw_by_id.contains_key(**id))
            .copied()
            .unwrap_or("unknown");
        bail!("fork source is missing raw record {missing}");
    }

    let ancestry = match leaf_id {
        Some(leaf_id) => tree.branch(Some(leaf_id)),
        None => Vec::new(),
    };
    let mut retained_entries = Vec::new();
    let mut rows = Vec::new();
    let mut retained_ids = HashSet::new();
    let mut retained_leaf_id: Option<String> = None;
    for entry in ancestry {
        if entry.entry_type == "label" || entry.entry_type == "checkpoint" {
            continue;
        }
        let mut row = (*raw_by_id
            .get(entry.id.as_str())
            .ok_or_else(|| anyhow!("fork source is missing raw ancestry record {}", entry.id))?)
        .clone();
        let object = row
            .as_object_mut()
            .ok_or_else(|| anyhow!("fork source raw record {} is not an object", entry.id))?;
        object.insert(
            "parentId".to_owned(),
            retained_leaf_id.clone().map_or(Value::Null, Value::String),
        );
        retained_leaf_id = Some(entry.id.clone());
        retained_ids.insert(entry.id.clone());
        retained_entries.push(entry);
        rows.push(row);
    }

    let mut allocation_ids = tree
        .entries
        .iter()
        .map(|entry| entry.id.clone())
        .collect::<HashSet<_>>();
    let mut used_ids = retained_ids.clone();
    let mut label_parent_id = retained_leaf_id.clone();
    let mut labels = Vec::new();
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
        labels.push(json!({
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
    Ok(RetainedBranch {
        rows,
        used_ids,
        leaf_id: retained_leaf_id,
        has_assistant,
        session_name,
        labels,
    })
}
fn create_branched_session_from_tree(
    source_path: &Path,
    tree: SessionTree,
    leaf_id: &str,
    session_dir: Option<&Path>,
) -> Result<SessionRecorder> {
    create_forked_session_from_tree(source_path, tree, None, Some(leaf_id), session_dir, false)
}

fn create_forked_session_from_tree(
    source_path: &Path,
    tree: SessionTree,
    raw_records: Option<Vec<Value>>,
    leaf_id: Option<&str>,
    session_dir: Option<&Path>,
    persist_immediately: bool,
) -> Result<SessionRecorder> {
    let raw_records = match raw_records {
        Some(records) => records,
        None => tree
            .entries
            .iter()
            .map(serde_json::to_value)
            .collect::<std::result::Result<Vec<_>, _>>()?,
    };
    let retained = retain_branch(&tree, &raw_records, leaf_id)?;
    let directory = session_dir.map_or_else(
        || {
            source_path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| default_session_dir(&tree.header.cwd))
        },
        absolute_path,
    );
    let lease = open_session_store_lease(&directory, RootCreation::CreateDirectory)?;
    let id = Uuid::now_v7().to_string();
    let timestamp = iso_now();
    let path = new_session_path(&directory, &timestamp, &id, false);
    let parent_session = Some(source_path.to_string_lossy().into_owned());
    let mut pending = vec![json!({
        "type": "session",
        "version": CURRENT_SESSION_VERSION,
        "id": id,
        "timestamp": timestamp,
        "cwd": tree.header.cwd,
        "parentSession": parent_session,
    })];
    pending.extend(retained.rows);
    pending.extend(retained.labels);

    let recorder = SessionRecorder {
        inner: Arc::new(Mutex::new(RecorderState {
            path,
            file_identity: None,
            id,
            timestamp,
            cwd: tree.header.cwd,
            parent_session,
            last_id: retained.leaf_id.clone(),
            active_leaf_id: retained.leaf_id,
            revision: 0,
            used_ids: retained.used_ids,
            pending,
            requires_separator: false,
            file: None,
            lease: Some(lease),
            flushed: false,
            has_assistant: retained.has_assistant,
            session_name: retained.session_name,
            durable: persist_immediately,
        })),
    };
    if persist_immediately {
        recorder.persist_now_durable()?;
    } else if retained.has_assistant {
        persist(&mut recorder.inner.lock())?;
    }
    Ok(recorder)
}


pub(crate) fn create_durable_forked_child(
    fork: ParentForkPoint,
    child_root: &CapabilityRootLease,
    child_session_id: &str,
) -> Result<SessionRecorder> {
    validate_session_id(child_session_id)?;
    let retained = retain_branch(
        &fork.tree,
        &fork.raw_records,
        fork.parent_active_leaf_id.as_deref(),
    )?;
    let timestamp = iso_now();
    let path = new_session_path(
        child_root.display_root(),
        &timestamp,
        child_session_id,
        true,
    );
    let parent_session = Some(fork.parent_session_path.to_string_lossy().into_owned());
    let mut pending = vec![json!({
        "type": "session",
        "version": CURRENT_SESSION_VERSION,
        "id": child_session_id,
        "timestamp": timestamp,
        "cwd": fork.tree.header.cwd,
        "parentSession": parent_session,
    })];
    pending.extend(retained.rows);
    pending.extend(retained.labels);

    let recorder = SessionRecorder {
        inner: Arc::new(Mutex::new(RecorderState {
            path,
            file_identity: None,
            id: child_session_id.to_owned(),
            timestamp,
            cwd: fork.tree.header.cwd,
            parent_session,
            last_id: retained.leaf_id.clone(),
            active_leaf_id: retained.leaf_id,
            revision: 0,
            used_ids: retained.used_ids,
            pending,
            requires_separator: false,
            file: None,
            lease: Some(child_root.clone()),
            flushed: false,
            has_assistant: retained.has_assistant,
            session_name: retained.session_name,
            durable: true,
        })),
    };
    if let Err(error) = recorder.persist_now_durable() {
        if is_already_exists(&error) {
            bail!("Session already exists with id '{child_session_id}'");
        }
        return Err(error).context("persisting durable forked child session");
    }
    Ok(recorder)
}

#[cfg(test)]
mod session_directory_compat_tests {
    use super::*;
    use std::fs;

    #[test]
    fn explicit_session_root_remains_authoritative() {
        let cwd = tempfile::tempdir().expect("cwd");
        let selected_dir = tempfile::tempdir().expect("selected session root");
        let recorder = start_session_in(
            cwd.path(),
            None,
            None,
            Some(selected_dir.path()),
            Some("explicit-session-root"),
            None,
        )
        .expect("start session in explicit root");

        assert_eq!(
            recorder.path().parent(),
            Some(selected_dir.path()),
            "agent/config root precedence must not replace an explicit session root",
        );
    }

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
            branch
                .tree()
                .expect("branch tree")
                .header
                .parent_session
                .as_deref(),
            Some(source_path.to_string_lossy().as_ref())
        );
    }

    #[test]
    fn prepared_fork_excludes_selected_user_message() {
        let cwd = tempfile::tempdir().expect("cwd");
        let sessions = tempfile::tempdir().expect("sessions");
        let branches = tempfile::tempdir().expect("branches");
        let source = start_session_in(
            cwd.path(),
            None,
            Some("off"),
            Some(sessions.path()),
            Some("prepared-fork-source"),
            None,
        )
        .expect("source session");
        let first = source
            .record_message(&Message::user_text("first", 0))
            .expect("first user message");
        let second = source
            .record_message(&Message::user_text("second", 1))
            .expect("second user message");
        source.persist_now().expect("persist source");
        let source_path = source.path();
        source.close().expect("close source");

        let branch = fork_prepared_session_at_user(
            PreparedSessionResume::prepare_path(&source_path).expect("prepare source"),
            &second,
            Some(branches.path()),
        )
        .expect("fork before second prompt");
        let tree = branch.tree().expect("branch tree");
        assert_eq!(
            tree.entries
                .iter()
                .filter_map(|entry| entry.message.as_ref())
                .count(),
            1
        );
        assert_eq!(tree.active_leaf_id.as_deref(), Some(first.as_str()));

        let first_user_prefix = fork_prepared_session_at_user(
            PreparedSessionResume::prepare_path(&source_path).expect("prepare first-user source"),
            &first,
            Some(branches.path()),
        )
        .expect("fork before first prompt");
        let first_user_tree = first_user_prefix.tree().expect("first-user prefix tree");
        assert!(
            first_user_tree
                .entries
                .iter()
                .all(|entry| entry.message.is_none()),
            "forking the first user excludes that prompt"
        );
        assert!(
            first_user_tree
                .entries
                .iter()
                .any(|entry| entry.entry_type == "thinking_level_change"),
            "first user under thinking_level_change must retain that ancestry"
        );
        assert_eq!(
            first_user_tree.header.parent_session.as_deref(),
            Some(source_path.to_string_lossy().as_ref()),
        );
    }

    #[test]
    fn true_root_user_fork_is_header_only_without_synthetic_state() {
        let cwd = tempfile::tempdir().expect("cwd");
        let sessions = tempfile::tempdir().expect("sessions");
        let branches = tempfile::tempdir().expect("branches");
        let source = start_session_in(
            cwd.path(),
            None,
            None,
            Some(sessions.path()),
            Some("true-root-source"),
            None,
        )
        .expect("source session");
        let first = source
            .record_message(&Message::user_text("root prompt", 0))
            .expect("root user");
        source.persist_now_durable().expect("persist source");
        let source_path = source.path();
        let MaterializedFork {
            recorder: empty,
            editor_text,
        } = fork_live_session_at_user(&source, &first, Some(branches.path())).expect("true root");
        assert_eq!(editor_text, "root prompt");
        source.close().expect("close source");

        let empty_tree = empty.tree().expect("empty branch tree");
        assert!(
            empty_tree
                .entries
                .iter()
                .all(|entry| entry.message.is_none())
        );
        assert_eq!(
            empty_tree.header.parent_session.as_deref(),
            Some(source_path.to_string_lossy().as_ref()),
        );
        assert!(empty.file_identity().is_some());
        let raw = fs::read_to_string(empty.path()).expect("read root fork jsonl");
        let rows: Vec<Value> = raw
            .lines()
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_str(line).expect("parse root fork row"))
            .collect();
        assert_eq!(rows.len(), 1, "true root fork must be header-only: {raw}");
        assert_eq!(rows[0]["type"], "session");
        assert!(rows.iter().all(|row| row["type"] != "model_change"));
        assert!(rows.iter().all(|row| row["type"] != "thinking_level_change"));
        assert!(rows.iter().all(|row| row["type"] != "message"));
    }

    fn raw_jsonl_rows(path: &Path) -> Vec<Value> {
        fs::read_to_string(path)
            .expect("read jsonl")
            .lines()
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_str(line).expect("parse jsonl row"))
            .collect()
    }

    fn user_texts(tree: &SessionTree) -> Vec<String> {
        tree.entries
            .iter()
            .filter_map(|entry| {
                let Message::User(user) = entry.message.as_ref()? else {
                    return None;
                };
                Some(
                    user.content
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::Text { text, .. } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect(),
                )
            })
            .collect()
    }

    #[test]
    fn fork_editor_text_concatenates_all_text_blocks_and_allows_image_only() {
        let mixed = Message::User(pi_ai::UserMessage {
            content: vec![
                ContentBlock::text("alpha"),
                ContentBlock::Image {
                    data: "aGVsbG8=".to_owned(),
                    mime_type: "image/png".to_owned(),
                },
                ContentBlock::text(""),
                ContentBlock::text(" omega"),
            ],
            timestamp: 7,
        });
        assert_eq!(fork_selected_editor_text(&mixed).as_deref(), Some("alpha omega"));

        let image_only = Message::User(pi_ai::UserMessage {
            content: vec![ContentBlock::Image {
                data: "aGVsbG8=".to_owned(),
                mime_type: "image/png".to_owned(),
            }],
            timestamp: 8,
        });
        assert_eq!(fork_selected_editor_text(&image_only).as_deref(), Some(""));
        assert_eq!(fork_selected_editor_text(&Message::Assistant(pi_ai::AssistantMessage::pending(&Model::default()))), None);
    }

    #[test]
    fn root_after_reset_leaf_is_header_only_without_prior_branch_prefix() {
        let cwd = tempfile::tempdir().expect("cwd");
        let sessions = tempfile::tempdir().expect("sessions");
        let branches = tempfile::tempdir().expect("branches");
        let source = start_session_in(
            cwd.path(),
            Some(&Model::default()),
            Some("off"),
            Some(sessions.path()),
            Some("reset-root-source"),
            None,
        )
        .expect("source");
        source
            .record_message(&Message::user_text("abandoned", 0))
            .expect("abandoned root");
        source.reset_leaf();
        let later_root = source
            .record_message(&Message::user_text("later root", 1))
            .expect("later root");
        source.persist_now_durable().expect("persist source");
        let source_path = source.path();

        let MaterializedFork {
            recorder: child,
            editor_text,
        } = fork_live_session_at_user(&source, &later_root, Some(branches.path()))
            .expect("fork later root");
        assert_eq!(editor_text, "later root");
        let rows = raw_jsonl_rows(&child.path());
        assert_eq!(
            rows.len(),
            1,
            "later root after reset_leaf must not copy abandoned ancestry: {rows:?}"
        );
        assert_eq!(rows[0]["type"], "session");
        assert_eq!(
            rows[0]["parentSession"].as_str(),
            Some(source_path.to_string_lossy().as_ref())
        );
        assert!(user_texts(&child.tree().expect("child tree")).is_empty());
        assert!(rows.iter().all(|row| row["type"] != "model_change"));
        assert!(rows.iter().all(|row| row["type"] != "thinking_level_change"));
    }

    #[test]
    fn nested_fork_raw_jsonl_preserves_ancestry_order_ids_and_tool_pairs() {
        let cwd = tempfile::tempdir().expect("cwd");
        let sessions = tempfile::tempdir().expect("sessions");
        let branches = tempfile::tempdir().expect("branches");
        let model = Model::default();
        let source = start_session_in(
            cwd.path(),
            Some(&model),
            Some("off"),
            Some(sessions.path()),
            Some("spectrum-source"),
            None,
        )
        .expect("source");
        let first = source
            .record_message(&Message::user_text("first question", 0))
            .expect("first user");
        let mut assistant = pi_ai::AssistantMessage::pending(&model);
        assistant.content = vec![ContentBlock::ToolCall(pi_ai::ToolCall {
            id: "call-read".to_owned(),
            name: "read".to_owned(),
            arguments: json!({"path": "notes.md"}),
            thought_signature: None,
        })];
        let assistant_id = source
            .record_message(&Message::Assistant(assistant))
            .expect("assistant tool call");
        let tool_result_id = source
            .record_message(&Message::ToolResult(pi_ai::ToolResultMessage {
                tool_call_id: "call-read".to_owned(),
                tool_name: "read".to_owned(),
                content: vec![ContentBlock::text("file body")],
                usage: None,
                details: None,
                added_tool_names: Vec::new(),
                is_error: false,
                timestamp: 2,
            }))
            .expect("tool result");
        let todo_id = source
            .record_todo_snapshot(&crate::TodoState {
                phases: vec![crate::TodoPhase {
                    name: "Build".to_owned(),
                    tasks: vec![crate::TodoItem {
                        id: "task-a".to_owned(),
                        content: "ship".to_owned(),
                        status: crate::TodoStatus::Pending,
                        depends_on: Vec::new(),
                        ready: true,
                        waiting_on: Vec::new(),
                        agent: None,
                    }],
                }],
                storage: crate::TodoStorage::Session,
            })
            .expect("todo");
        let goal_id = source
            .record_custom_entry(
                crate::GOAL_SESSION_CUSTOM_TYPE,
                Some(json!({"kind": "set", "objective": "ship it"})),
            )
            .expect("goal");
        let name_id = source
            .record_session_name("spectrum")
            .expect("session name")
            .and(source.last_entry_id());
        let custom_id = source
            .record_custom_entry("extension.state", Some(json!({"cursor": 7})))
            .expect("custom");
        source
            .record_label(&first, Some("cleared"))
            .expect("temporary label");
        source
            .record_label(&first, None)
            .expect("clear first label");
        source
            .record_label(&assistant_id, Some("old"))
            .expect("old assistant label");
        source
            .record_label(&assistant_id, Some("current"))
            .expect("current assistant label");
        let selected = source
            .record_message(&Message::user_text("second question", 3))
            .expect("selected user");
        source.persist_now_durable().expect("persist source");
        let source_path = source.path();
        let source_rows = raw_jsonl_rows(&source_path);

        let MaterializedFork {
            recorder: live,
            editor_text,
        } = fork_live_session_at_user(&source, &selected, Some(branches.path()))
            .expect("live nested fork");
        assert_eq!(editor_text, "second question");
        source.close().expect("close source");

        let prepared = materialize_fork_at_user(
            PreparedSessionResume::prepare_path(&source_path)
                .expect("prepare source")
                .into(),
            &selected,
            Some(branches.path()),
        )
        .expect("prepared nested fork");
        assert_eq!(prepared.editor_text, "second question");

        for child in [&live, &prepared.recorder] {
            let rows = raw_jsonl_rows(&child.path());
            assert_eq!(rows[0]["type"], "session");
            assert_eq!(
                rows[0]["parentSession"].as_str(),
                Some(source_path.to_string_lossy().as_ref())
            );
            let retained_source = source_rows
                .iter()
                .skip(1)
                .filter(|row| row["type"] != "label")
                .take_while(|row| row["id"] != selected)
                .cloned()
                .collect::<Vec<_>>();
            assert_eq!(
                &rows[1..1 + retained_source.len()],
                retained_source.as_slice(),
                "fork must preserve every retained raw record exactly, including ids, parentIds, timestamps, and payload order"
            );
            let types: Vec<&str> = rows.iter().map(|row| row["type"].as_str().unwrap()).collect();
            assert_eq!(
                &types[..types.len() - 1],
                [
                    "session",
                    "model_change",
                    "thinking_level_change",
                    "message",
                    "message",
                    "message",
                    "todo_snapshot",
                    "custom",
                    "session_info",
                    "custom",
                ]
            );
            assert_eq!(types.last().copied(), Some("label"));
            assert!(types.iter().all(|entry_type| *entry_type != "checkpoint"));
            let retained_rows = &rows[1..1 + retained_source.len()];
            let expected_ids = retained_source
                .iter()
                .map(|row| row["id"].as_str().expect("source id"))
                .collect::<Vec<_>>();
            let actual_ids = retained_rows
                .iter()
                .map(|row| row["id"].as_str().expect("child id"))
                .collect::<Vec<_>>();
            assert_eq!(actual_ids, expected_ids);
            assert_eq!(actual_ids[2], first);
            assert_eq!(actual_ids[3], assistant_id);
            assert_eq!(actual_ids[4], tool_result_id);
            assert_eq!(actual_ids[5], todo_id);
            assert_eq!(actual_ids[6], goal_id);
            if let Some(name_id) = name_id.as_deref() {
                assert_eq!(actual_ids[7], name_id);
            }
            assert_eq!(actual_ids[8], custom_id);
            let expected_parents = std::iter::once(None)
                .chain(expected_ids.iter().map(|id| Some(*id)))
                .take(expected_ids.len())
                .collect::<Vec<_>>();
            let actual_parents = retained_rows
                .iter()
                .map(|row| row["parentId"].as_str())
                .collect::<Vec<_>>();
            assert_eq!(actual_parents, expected_parents, "exact retained chain");
            assert_eq!(retained_rows[2]["message"]["timestamp"], 0);
            assert_eq!(retained_rows[3]["message"]["content"][0]["type"], "toolCall");
            assert_eq!(retained_rows[3]["message"]["content"][0]["id"], "call-read");
            assert_eq!(retained_rows[3]["message"]["content"][0]["name"], "read");
            assert_eq!(retained_rows[3]["message"]["content"][0]["arguments"], json!({"path": "notes.md"}));
            assert_eq!(retained_rows[4]["message"]["toolCallId"], "call-read");
            assert_eq!(retained_rows[4]["message"]["toolName"], "read");
            assert_eq!(retained_rows[4]["message"]["timestamp"], 2);
            assert_eq!(retained_rows[4]["message"]["content"], json!([{"type":"text","text":"file body"}]));
            assert_eq!(retained_rows[6]["customType"], crate::GOAL_SESSION_CUSTOM_TYPE);
            assert_eq!(retained_rows[6]["data"], json!({"kind": "set", "objective": "ship it"}));
            assert_eq!(retained_rows[8]["customType"], "extension.state");
            assert_eq!(retained_rows[8]["data"], json!({"cursor": 7}));
            assert!(!actual_ids.contains(&selected.as_str()));
            let unique: HashSet<&str> = actual_ids.iter().copied().collect();
            assert_eq!(unique.len(), actual_ids.len());
            let tree = child.tree().expect("child tree");
            assert_eq!(user_texts(&tree), vec!["first question".to_owned()]);
            assert_eq!(
                tree.latest_todo_state()
                    .expect("todo")
                    .phases[0]
                    .tasks[0]
                    .id,
                "task-a"
            );
            assert_eq!(tree.session_name().as_deref(), Some("spectrum"));
            assert_eq!(
                tree.labels
                    .get(&assistant_id)
                    .map(|label| label.label.as_str()),
                Some("current")
            );
            assert!(tree.labels.get(&first).is_none());
            let label_rows: Vec<&Value> = rows.iter().filter(|row| row["type"] == "label").collect();
            assert_eq!(label_rows.len(), 1);
            assert_eq!(label_rows[0]["parentId"], custom_id);
            assert_eq!(label_rows[0]["targetId"], assistant_id);
            assert_eq!(label_rows[0]["label"], "current");
            let source_current_label = source_rows
                .iter()
                .find(|row| row["type"] == "label" && row["targetId"] == assistant_id && row["label"] == "current")
                .expect("source current label");
            assert_eq!(label_rows[0]["timestamp"], source_current_label["timestamp"]);
            assert_ne!(label_rows[0]["id"], "current");
        }
    }

    #[test]
    fn fork_rejects_corrupt_raw_record_mapping() {
        let cwd = tempfile::tempdir().expect("cwd");
        let sessions = tempfile::tempdir().expect("sessions");
        let branches = tempfile::tempdir().expect("branches");
        let source = start_session_in(
            cwd.path(),
            None,
            None,
            Some(sessions.path()),
            Some("corrupt-raw-source"),
            None,
        )
        .expect("source");
        let retained = source
            .record_message(&Message::user_text("retained", 0))
            .expect("retained user");
        let selected = source
            .record_message(&Message::user_text("selected", 1))
            .expect("selected user");
        source.persist_now_durable().expect("persist source");

        let mut snapshot: ForkSourceSnapshot = source.capture_fork_point().expect("capture").into();
        let raw = snapshot
            .raw_records
            .iter_mut()
            .find(|row| row["id"] == retained)
            .expect("retained raw row");
        raw["type"] = Value::String("custom".to_owned());

        let error = materialize_fork_at_user(snapshot, &selected, Some(branches.path()))
            .expect_err("raw/typed mismatch must fail closed");
        assert!(
            error.to_string().contains("type mismatch"),
            "unexpected error: {error:#}"
        );
    }
    #[test]
    fn fork_preserves_compaction_active_leaf_and_validated_writer() {
        let cwd = tempfile::tempdir().expect("cwd");
        let sessions = tempfile::tempdir().expect("sessions");
        let branches = tempfile::tempdir().expect("branches");
        let model = Model::default();
        let source = start_session_in(
            cwd.path(),
            Some(&model),
            Some("off"),
            Some(sessions.path()),
            Some("compaction-fork-source"),
            None,
        )
        .expect("source");
        let first = source
            .record_message(&Message::user_text("first question", 0))
            .expect("first user");
        let mut assistant = pi_ai::AssistantMessage::pending(&model);
        assistant.content = vec![ContentBlock::text("compacted reply")];
        let assistant_id = source
            .record_message(&Message::Assistant(assistant))
            .expect("assistant");
        let compaction_usage = Usage {
            input: 21,
            output: 5,
            total_tokens: 26,
            ..Usage::default()
        };
        let compaction_id = source
            .record_compaction_metadata(
                "compacted transcript",
                Some(&first),
                1234,
                &[],
                Some(&json!({"modifiedFiles": ["src/main.rs"]})),
                Some(&compaction_usage),
                Some(false),
            )
            .expect("compaction");
        let selected = source
            .record_message(&Message::user_text("resume question", 1))
            .expect("selected user");
        source.persist_now_durable().expect("persist source");
        let source_path = source.path();
        let source_compaction = raw_jsonl_rows(&source_path)
            .into_iter()
            .find(|row| row["id"] == compaction_id)
            .expect("source compaction row");

        let MaterializedFork {
            recorder: live,
            editor_text,
        } = fork_live_session_at_user(&source, &selected, Some(branches.path()))
            .expect("live fork");
        assert_eq!(editor_text, "resume question");
        source.close().expect("close source");

        let prepared = fork_prepared_session_at_user(
            PreparedSessionResume::prepare_path(&source_path).expect("prepare source"),
            &selected,
            Some(branches.path()),
        )
        .expect("prepared fork");

        for child in [&live, &prepared] {
            let rows = raw_jsonl_rows(&child.path());
            let types: Vec<&str> = rows.iter().map(|row| row["type"].as_str().unwrap()).collect();
            assert_eq!(
                types,
                [
                    "session",
                    "model_change",
                    "thinking_level_change",
                    "message",
                    "message",
                    "compaction",
                ],
                "fork must retain the authoritative prefix including the compaction: {rows:?}"
            );
            assert_eq!(
                rows[5], source_compaction,
                "compaction row must be copied verbatim, including summary, tokensBefore, firstKeptEntryId, details, usage, and fromHook"
            );
            assert_eq!(rows[5]["parentId"], assistant_id);
            assert_eq!(rows[5]["firstKeptEntryId"], first);
            assert!(rows.iter().all(|row| row["id"] != selected));

            let tree = child.tree().expect("child tree");
            assert_eq!(
                tree.active_leaf_id.as_deref(),
                Some(compaction_id.as_str()),
                "compaction must remain the child's active leaf"
            );
            assert_eq!(user_texts(&tree), vec!["first question".to_owned()]);
            let context = tree.build_context(None);
            assert_eq!(context.messages.len(), 3);
            assert!(
                matches!(
                    context.messages.get(1),
                    Some(Message::User(user))
                        if user.content.iter().any(|block| matches!(
                            block,
                            ContentBlock::Text { text, .. } if text == "first question"
                        ))
                ),
                "retained ancestry must follow the compaction summary: {:?}",
                context.messages.get(1)
            );
            assert!(matches!(context.messages.get(2), Some(Message::Assistant(_))));
            assert!(
                matches!(
                    context.messages.first(),
                    Some(Message::CompactionSummary(summary))
                        if summary.summary == "compacted transcript" && summary.tokens_before == 1234
                ),
                "forked child must rebuild the compaction summary context: {:?}",
                context.messages.first()
            );

            // Observable materialization assertions only: the child file holds
            // exactly one header row and the append writer descriptor returned
            // by the recorder validates against that file. Writer exclusivity
            // is not asserted — no session_store open path enforces it.
            let (writer_id, writer_path, identity) =
                child.writer_info().expect("materialized append writer validates");
            assert_eq!(writer_id, child.id());
            assert_eq!(writer_path, child.path());
            assert_eq!(Some(&identity), child.file_identity().as_ref());
        }
    }

    #[test]
    fn fork_survives_parent_deletion_and_independent_append() {
        let cwd = tempfile::tempdir().expect("cwd");
        let sessions = tempfile::tempdir().expect("sessions");
        let branches = tempfile::tempdir().expect("branches");
        let source = start_session_in(
            cwd.path(),
            Some(&Model::default()),
            Some("off"),

            Some(sessions.path()),
            Some("delete-parent-source"),
            None,
        )
        .expect("source");
        let first = source
            .record_message(&Message::user_text("keep me", 0))
            .expect("first");
        let selected = source
            .record_message(&Message::user_text("fork here", 1))
            .expect("selected");
        source.persist_now_durable().expect("persist source");
        let source_path = source.path();

        let MaterializedFork {
            recorder: child,
            editor_text,
        } = fork_live_session_at_user(&source, &selected, Some(branches.path()))
            .expect("live fork");
        assert_eq!(editor_text, "fork here");
        let child_path = child.path();
        let child_id = child.id();
        source.close().expect("close source");
        fs::remove_file(&source_path).expect("delete parent jsonl");
        assert!(!source_path.exists());

        let reopened = resume_session(&child_path).expect("reopen child after parent deletion");
        assert_eq!(reopened.id(), child_id);
        assert_eq!(
            user_texts(&reopened.tree().expect("reopened tree")),
            vec!["keep me".to_owned()]
        );
        let appended = reopened
            .record_message(&Message::user_text("child-only", 2))
            .expect("append after parent deletion");
        reopened.persist_now_durable().expect("persist child append");
        let tree = load_session_tree(&child_path).expect("reload child");
        assert_eq!(
            user_texts(&tree),
            vec!["keep me".to_owned(), "child-only".to_owned()]
        );
        assert_eq!(tree.active_leaf_id.as_deref(), Some(appended.as_str()));
        assert!(
            tree.entries.iter().any(|entry| entry.id == first),
            "parent ancestry id {first} must survive parent deletion"
        );
        assert!(
            tree.entries.iter().all(|entry| entry.id != selected),
            "selected prompt must stay absent after parent deletion"
        );
    }

    #[test]
    fn live_fork_reads_retained_parent_handle_not_replaced_path() {
        let cwd = tempfile::tempdir().expect("cwd");
        let sessions = tempfile::tempdir().expect("sessions");
        let branches = tempfile::tempdir().expect("branches");
        let source = start_session_in(
            cwd.path(),
            None,
            None,
            Some(sessions.path()),
            Some("no-reopen-source"),
            None,
        )
        .expect("source");
        let first = source
            .record_message(&Message::user_text("captured", 0))
            .expect("first");
        let selected = source
            .record_message(&Message::user_text("select me", 1))
            .expect("selected");
        source.persist_now_durable().expect("persist source");
        let source_path = source.path();
        let retained_inode = sessions.path().join("retained-parent.jsonl");
        fs::rename(&source_path, &retained_inode).expect("move retained parent inode");
        let decoy = json!({
            "type": "session",
            "version": CURRENT_SESSION_VERSION,
            "id": "decoy",
            "timestamp": "2026-01-01T00:00:00.000Z",
            "cwd": cwd.path(),
        });
        fs::write(
            &source_path,
            format!(
                "{}\n{}\n",
                serde_json::to_string(&decoy).expect("decoy header"),
                serde_json::to_string(&json!({
                    "type": "message",
                    "id": "decoy-user",
                    "parentId": null,
                    "timestamp": "2026-01-01T00:00:01.000Z",
                    "message": Message::user_text("DECOY", 9),
                }))
                .expect("decoy user")
            ),
        )
        .expect("replace parent path with decoy inode");

        let MaterializedFork {
            recorder: child,
            editor_text,
        } = fork_live_session_at_user(&source, &selected, Some(branches.path()))
            .expect("materialize through retained parent handle");
        assert_eq!(editor_text, "select me");
        let texts = user_texts(&child.tree().expect("child tree"));
        assert_eq!(texts, vec!["captured".to_owned()]);
        assert!(!texts.iter().any(|text| text == "DECOY"));
        assert!(
            child
                .tree()
                .expect("child tree")
                .entries
                .iter()
                .any(|entry| entry.id == first)
        );
    }

}

pub fn fork_session_in(
    source_path: impl AsRef<Path>,
    target_cwd: impl AsRef<Path>,
    session_dir: Option<&Path>,
    session_id: Option<&str>,
) -> Result<SessionRecorder> {
    let prepared = PreparedSessionResume::prepare_path(source_path)?;
    fork_prepared_session_in(prepared, target_cwd, session_dir, session_id)
}

pub fn fork_prepared_session_in(
    prepared: PreparedSessionResume,
    target_cwd: impl AsRef<Path>,
    session_dir: Option<&Path>,
    session_id: Option<&str>,
) -> Result<SessionRecorder> {
    let source_path = prepared.path;
    let tree = prepared.tree;
    let target_cwd = absolute_path(target_cwd.as_ref());
    let directory = session_dir.map_or_else(|| default_session_dir(&target_cwd), absolute_path);
    let lease = open_session_store_lease(&directory, RootCreation::CreateDirectory)?;
    let has_explicit_id = session_id.is_some();
    let id = match session_id {
        Some(id) => {
            validate_session_id(id)?;
            id.to_owned()
        }
        None => Uuid::now_v7().to_string(),
    };
    if list_sessions_in(&target_cwd, Some(&directory))
        .iter()
        .any(|session| session.id == id)
    {
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
    let mut label_parent_id = leaf_id.clone();
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
            "type": "label", "id": label_id, "parentId": label_parent_id,
            "timestamp": resolved.timestamp, "targetId": target_id, "label": resolved.label,
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
            cwd: target_cwd,
            parent_session,
            file_identity: None,
            last_id: leaf_id.clone(),
            active_leaf_id: leaf_id,
            revision: 0,
            used_ids,
            pending,
            requires_separator: false,
            file: None,
            flushed: false,
            has_assistant,
            session_name,
            durable: false,
            lease: Some(lease),
        })),
    };
    if has_explicit_id {
        recorder
            .persist_now()
            .context("reserving explicit fork session id")?;
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
    start_durable_child_session_in_with_kind(
        cwd,
        model,
        thinking_level,
        session_dir,
        session_id,
        parent_session,
        None,
    )
}

pub fn start_durable_child_session_in_with_kind(
    cwd: impl AsRef<Path>,
    model: Option<&Model>,
    thinking_level: Option<&str>,
    session_dir: &Path,
    session_id: Option<&str>,
    parent_session: &Path,
    session_kind: Option<SessionKind>,
) -> Result<SessionRecorder> {
    let cwd = absolute_path(cwd.as_ref());
    let directory = absolute_path(session_dir);
    let lease = open_session_store_lease(&directory, RootCreation::CreateDirectory)?;
    let has_explicit_id = session_id.is_some();
    let id = match session_id {
        Some(id) => {
            validate_session_id(id)?;
            id.to_owned()
        }
        None => Uuid::now_v7().to_string(),
    };
    let timestamp = iso_now();
    let path = new_session_path(&directory, &timestamp, &id, has_explicit_id);
    let parent_session_str = parent_session.to_string_lossy().into_owned();
    let mut header = json!({
        "type": "session", "version": CURRENT_SESSION_VERSION, "id": id,
        "timestamp": timestamp, "cwd": cwd, "parentSession": parent_session_str,
    });
    if let Some(kind) = session_kind.filter(|kind| kind.is_workflow_internal()) {
        header["sessionKind"] = json!(kind.header_value());
    }
    let recorder = SessionRecorder {
        inner: Arc::new(Mutex::new(RecorderState {
            path,
            id,
            timestamp,
            cwd,
            parent_session: Some(parent_session_str),
            file_identity: None,
            last_id: None,
            active_leaf_id: None,
            revision: 0,
            used_ids: HashSet::new(),
            pending: vec![header],
            requires_separator: false,
            file: None,
            flushed: false,
            lease: Some(lease),
            has_assistant: false,
            session_name: None,
            durable: true,
        })),
    };
    // Durable-append the header immediately so the file exists on disk.
    recorder
        .persist_now_durable()
        .context("reserving durable child session header")?;
    if let Some(model) = model {
        recorder.record_model_change(&model.provider, &model.id)?;
    }
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
    let path = absolute_path(path.as_ref());
    let capabilities = CapabilityFs::new().context("initializing session filesystem capability")?;
    let absolute = AbsolutePath::parse(path.clone())
        .map_err(|_| anyhow!("invalid session path {}", path.display()))?;
    let file = capabilities
        .open_exact_file(CapabilityRootPurpose::SessionStore, absolute)
        .with_context(|| format!("opening session {}", path.display()))?;
    let standard = file
        .try_clone_std()
        .with_context(|| format!("cloning session handle {}", path.display()))?;
    load_session_tree_from_file(standard, &path)
}

pub(crate) fn load_session_tree_from_file(mut file: File, path: &Path) -> Result<SessionTree> {
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

    // Fast path: a single-pass typed parse. Every record written by the
    // store deserializes directly into its typed shape (one allocation pass
    // from the raw bytes; no intermediate `serde_json::Value` DOM and no
    // second parse of nested messages, which more than doubles the cost on
    // large sessions). Any line that does not map to a known record shape
    // defers to the legacy lenient Value-based path below, keeping behavior
    // byte-identical for foreign or hand-edited files.

    if let Some(tree) = load_session_tree_typed(&mut file, path)? {
        return Ok(tree);
    }

    #[cfg(test)]
    LENIENT_LOAD_PATH_HITS.with(|hits| hits.set(hits.get() + 1));
    file.seek(SeekFrom::Start(0))
        .with_context(|| format!("seeking session {} for parse", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    let mut values = Vec::new();
    let mut line_number = 0usize;
    let mut bytes_read = 0u64;
    while let Some(consumed) =
        read_bounded_session_line(&mut reader, &mut line).with_context(|| {
            format!(
                "reading session {} line {}",
                path.display(),
                line_number + 1
            )
        })?
    {
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
        let value = serde_json::from_slice::<Value>(trimmed)
            .with_context(|| format!("parsing session {} line {}", path.display(), line_number))?;
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
fn read_session_values_from_file(mut file: File, path: &Path) -> Result<Vec<Value>> {
    file.seek(SeekFrom::Start(0))
        .with_context(|| format!("seeking session {} for raw capture", path.display()))?;
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
    while let Some(consumed) =
        read_bounded_session_line(&mut reader, &mut line).with_context(|| {
            format!(
                "reading session {} line {}",
                path.display(),
                line_number + 1
            )
        })?
    {
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
        let value = serde_json::from_slice::<Value>(trimmed)
            .with_context(|| format!("parsing session {} line {}", path.display(), line_number))?;
        if !value.is_object() {
            bail!(
                "session {} line {} must be a JSON object",
                path.display(),
                line_number
            );
        }
        values.push(value);
    }
    Ok(values)
}

/// Single-pass typed parse attempt for a session file. Returns `Ok(None)` when
/// any line does not deserialize into a known record shape; the caller must
/// then fall back to the legacy lenient Value-based parse so foreign or
/// hand-edited files keep loading with identical tolerance and error
/// reporting.
fn load_session_tree_typed(file: &mut File, path: &Path) -> Result<Option<SessionTree>> {
    file.seek(SeekFrom::Start(0))
        .with_context(|| format!("seeking session {} for parse", path.display()))?;
    let mut reader = BufReader::new(
        file.try_clone()
            .with_context(|| format!("cloning retained session handle {}", path.display()))?,
    );
    let mut line = Vec::new();
    let mut header = None;
    let mut records = Vec::new();
    let mut line_number = 0usize;
    let mut bytes_read = 0u64;
    while let Some(consumed) =
        read_bounded_session_line(&mut reader, &mut line).with_context(|| {
            format!(
                "reading session {} line {}",
                path.display(),
                line_number + 1
            )
        })?
    {
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
        if header.is_none() {
            // The header line is parsed as an untyped object so its full
            // metadata set (e.g. parentSession) is preserved verbatim.
            let value = match serde_json::from_slice::<Value>(trimmed) {
                Ok(value) => value,
                Err(_) => return Ok(None),
            };
            if !value.is_object() {
                return Ok(None);
            }
            header = Some(value);
            continue;
        }
        if records.len() >= MAX_SESSION_RECORDS {
            bail!(
                "session {} exceeds the 100000 record safety limit at line {}",
                path.display(),
                line_number
            );
        }
        match serde_json::from_slice::<SessionRecord>(trimmed) {
            Ok(record) => records.push(record),
            Err(_) => return Ok(None),
        }
    }
    let Some(header) = header else {
        return Ok(None);
    };
    Ok(Some(session_tree_from_records(path, &header, records)?))
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

/// Shared duplicate-entry guard for both session loaders (typed fast path and
/// lenient Value fallback). A repeated non-empty id would alias two
/// `SessionEntry` values through one `by_id` slot, producing self-parent loops
/// when the tree materializes and exponential rebuilds while the session
/// appends. The error names the offending id and its entries-based record
/// position (`entries.len() + 2`, with the header as record 1), so records
/// skipped by either loader do not advance it. This keeps both paths
/// consistent and deliberately never embeds the session file's local path.
fn reject_duplicate_entry_id(
    by_id: &HashMap<String, usize>,
    id: &str,
    record: usize,
) -> Result<()> {
    if !id.is_empty() && by_id.contains_key(id) {
        bail!(
            "duplicate session entry id {id:?} at record {record}; remove or rename the duplicate record and reload"
        );
    }
    Ok(())
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
        .ok_or_else(|| {
            anyhow!(
                "not a Pi session file (missing session header): {}",
                path.display()
            )
        })?;
    let id = nonempty_string(header_value, "id").ok_or_else(|| {
        anyhow!(
            "not a Pi session file (missing session id): {}",
            path.display()
        )
    })?;
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
    let mut skipped_idless = 0usize;
    let mut undecodable_messages = 0usize;
    for value in values.iter().skip(1) {
        let Some(object) = value.as_object() else {
            continue;
        };
        let Some(id) = nonempty_string(object, "id") else {
            skipped_idless = skipped_idless.saturating_add(1);
            continue;
        };
        let entry_type = nonempty_string(object, "type").unwrap_or_default();
        let is_todo_snapshot = entry_type == "todo_snapshot";
        let todo_state = match object.get("state") {
            Some(state) if is_todo_snapshot => {
                Some(serde_json::from_value(state.clone()).with_context(|| {
                    format!("decoding todo_snapshot {id} in {}", path.display())
                })?)
            }
            Some(state) => serde_json::from_value(state.clone()).ok(),
            None => None,
        };
        let todo_execution_status = if is_todo_snapshot {
            object
                .get("executionStatus")
                .cloned()
                .map(|value| {
                    serde_json::from_value(value).with_context(|| {
                        format!("decoding todo_snapshot {id} executionStatus in {}", path.display())
                    })
                })
                .transpose()?
        } else {
            None
        };
        let todo_failure_reason = if is_todo_snapshot {
            object
                .get("failureReason")
                .cloned()
                .map(|value| {
                    serde_json::from_value(value).with_context(|| {
                        format!("decoding todo_snapshot {id} failureReason in {}", path.display())
                    })
                })
                .transpose()?
        } else {
            None
        };
        let message = match object.get("message") {
            None => None,
            Some(raw) => match serde_json::from_value::<Message>(raw.clone()) {
                Ok(decoded) => Some(decoded),
                Err(_) => {
                    undecodable_messages = undecodable_messages.saturating_add(1);
                    None
                }
            },
        };
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
        let decoded_messages =
            reconstructed_message_count(&entry_type, message.as_ref(), object, &retained_tail);
        reconstructed_messages = reconstructed_messages
            .checked_add(decoded_messages)
            .ok_or_else(|| {
                anyhow!(
                    "session {} reconstructed message count overflow",
                    path.display()
                )
            })?;
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
            name: object
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_owned),
            label: nonempty_string(object, "label"),
            target_id: nonempty_string(object, "targetId"),
            todo_state,
            todo_execution_status,
            todo_failure_reason,
            from_id: nonempty_string(object, "fromId"),
            custom_type: nonempty_string(object, "customType"),
        };
        reject_duplicate_entry_id(&by_id, &id, entries.len() + 2)?;
        by_id.insert(id, entries.len());
        entries.push(entry);
    }
    let leaf_id = entries
        .iter()
        .rev()
        .find(|entry| entry.entry_type != "label" && entry.entry_type != "checkpoint")
        .map(|entry| entry.id.clone());
    let (children_by_parent, labels) = build_tree_indexes(&entries, &by_id);
    Ok(SessionTree {
        header,
        entries,
        active_leaf_id: leaf_id.clone(),
        leaf_id,
        skipped_idless,
        undecodable_messages,
        by_id,
        children_by_parent,
        labels,
    })
}
/// The entry id every typed record variant carries. Or-patterns keep the
/// single field access as one match, mirroring [`record_message_count`].
fn record_id(record: &SessionRecord) -> &str {
    match record {
        SessionRecord::Session { id, .. }
        | SessionRecord::Message { id, .. }
        | SessionRecord::ModelChange { id, .. }
        | SessionRecord::ThinkingLevelChange { id, .. }
        | SessionRecord::Compaction { id, .. }
        | SessionRecord::SessionInfo { id, .. }
        | SessionRecord::BranchSummary { id, .. }
        | SessionRecord::Custom { id, .. }
        | SessionRecord::CustomMessage { id, .. }
        | SessionRecord::TodoSnapshot { id, .. }
        | SessionRecord::Label { id, .. }
        | SessionRecord::Checkpoint { id, .. } => id,
    }
}

/// Number of model-visible messages a typed record reconstructs, mirroring
/// [`reconstructed_message_count`] for the typed fast path.
fn record_message_count(record: &SessionRecord) -> usize {
    match record {
        SessionRecord::Message { .. } => 1,
        SessionRecord::CustomMessage { custom_type, .. } => usize::from(!custom_type.is_empty()),
        SessionRecord::BranchSummary { summary, .. } => usize::from(!summary.is_empty()),
        SessionRecord::Compaction {
            summary,
            retained_tail,
            ..
        } => usize::from(!summary.is_empty()).saturating_add(retained_tail.len()),
        _ => 0,
    }
}

/// Shared empty-default [`SessionEntry`] shell; `entry_type` is always a
/// static literal for typed records, so no per-record allocation is incurred.
fn typed_entry(
    id: String,
    parent_id: Option<String>,
    entry_type: &'static str,
    timestamp: String,
) -> SessionEntry {
    SessionEntry {
        entry_type: entry_type.to_owned(),
        id,
        parent_id,
        timestamp,
        message: None,
        provider: None,
        model_id: None,
        thinking_level: None,
        summary: None,
        first_kept_entry_id: None,
        tokens_before: None,
        retained_tail: Vec::new(),
        content: None,
        display: None,
        details: None,
        usage: None,
        from_hook: None,
        data: None,
        name: None,
        label: None,
        target_id: None,
        todo_state: None,
        todo_execution_status: None,
        todo_failure_reason: None,
        from_id: None,
        custom_type: None,
    }
}

/// Build the session tree from a fully-typed record list (the fast path).
/// Mirrors [`session_tree_from_values`] field-for-field so restore semantics
/// are identical; fields the typed shapes do not model stay `None`. Records
/// whose id is empty are skipped exactly like the lenient path skips
/// `id`-less or empty-`id` rows, so neither loader ever indexes an empty id.
fn session_tree_from_records(
    path: &Path,
    header_value: &Value,
    records: Vec<SessionRecord>,
) -> Result<SessionTree> {
    #[cfg(test)]
    TYPED_LOAD_PATH_HITS.with(|hits| hits.set(hits.get() + 1));
    if records.len() > MAX_SESSION_RECORDS {
        bail!(
            "session {} exceeds the 100000 record safety limit at record {}",
            path.display(),
            MAX_SESSION_RECORDS + 1
        );
    }
    let header_object = header_value
        .as_object()
        .filter(|value| value.get("type").and_then(Value::as_str) == Some("session"))
        .ok_or_else(|| {
            anyhow!(
                "not a Pi session file (missing session header): {}",
                path.display()
            )
        })?;
    let id = nonempty_string(header_object, "id").ok_or_else(|| {
        anyhow!(
            "not a Pi session file (missing session id): {}",
            path.display()
        )
    })?;
    let header = SessionHeader {
        record_type: "session".to_owned(),
        version: header_object
            .get("version")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .unwrap_or(1),
        id,
        timestamp: nonempty_string(header_object, "timestamp").unwrap_or_default(),
        cwd: header_object
            .get("cwd")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .unwrap_or_default(),
        parent_session: nonempty_string(header_object, "parentSession"),
    };

    let mut entries = Vec::with_capacity(records.len());
    let mut by_id = HashMap::with_capacity(records.len());
    let mut reconstructed_messages = 0usize;
    let mut skipped_idless = 0usize;
    for record in records {
        // Mirrors the lenient path's `nonempty_string(object, "id")` filter:
        // records whose id is missing or empty are skipped before any message
        // accounting or indexing. Indexing an empty id would alias every such
        // record onto one `by_id[""]` slot, and `build_tree_indexes` would
        // then attach every `parentId: ""` record beneath each aliased
        // empty-id node, duplicating the subtree per alias (exponential
        // materialization on hand-edited files). Skipping keeps both loaders
        // on one id contract and leaves the writer's validated non-empty ids
        // untouched. Record numbering remains entries-based in both paths:
        // the first indexed entry is record 2, and skipped rows do not
        // advance the reported position.
        if record_id(&record).is_empty() {
            skipped_idless = skipped_idless.saturating_add(1);
            continue;
        }
        let decoded_messages = record_message_count(&record);
        reconstructed_messages = reconstructed_messages
            .checked_add(decoded_messages)
            .ok_or_else(|| {
                anyhow!(
                    "session {} reconstructed message count overflow",
                    path.display()
                )
            })?;
        if reconstructed_messages > MAX_RECONSTRUCTED_MESSAGES {
            bail!(
                "session {} exceeds the 100000 reconstructed message safety limit at record {}",
                path.display(),
                entries.len() + 2
            );
        }
        let entry = match record {
            SessionRecord::Session { id, timestamp, .. } => {
                typed_entry(id, None, "session", timestamp)
            }
            SessionRecord::Message {
                id,
                parent_id,
                timestamp,
                message,
            } => {
                let mut entry = typed_entry(id, parent_id, "message", timestamp);
                entry.message = Some(message);
                entry
            }
            SessionRecord::ModelChange {
                id,
                parent_id,
                timestamp,
                provider,
                model_id,
            } => {
                let mut entry = typed_entry(id, parent_id, "model_change", timestamp);
                // The lenient path treats empty provider/modelId (e.g. an
                // unconfigured default model) as absent; preserve that so the
                // typed fast path restores metadata identically.
                if !provider.is_empty() {
                    entry.provider = Some(provider);
                }
                if !model_id.is_empty() {
                    entry.model_id = Some(model_id);
                }
                entry
            }
            SessionRecord::ThinkingLevelChange {
                id,
                parent_id,
                timestamp,
                thinking_level,
            } => {
                let mut entry = typed_entry(id, parent_id, "thinking_level_change", timestamp);
                entry.thinking_level = Some(thinking_level);
                entry
            }
            SessionRecord::Compaction {
                id,
                parent_id,
                timestamp,
                summary,
                first_kept_entry_id,
                tokens_before,
                retained_tail,
                details,
                usage,
                from_hook,
            } => {
                let mut entry = typed_entry(id, parent_id, "compaction", timestamp);
                entry.summary = Some(summary);
                entry.first_kept_entry_id = first_kept_entry_id;
                entry.tokens_before = Some(tokens_before);
                entry.retained_tail = retained_tail;
                entry.details = details;
                entry.usage = usage;
                entry.from_hook = from_hook;
                entry
            }
            SessionRecord::SessionInfo {
                id,
                parent_id,
                timestamp,
                name,
            } => {
                let mut entry = typed_entry(id, parent_id, "session_info", timestamp);
                entry.name = Some(name);
                entry
            }
            SessionRecord::BranchSummary {
                id,
                parent_id,
                timestamp,
                from_id,
                summary,
                details,
                usage,
                from_hook,
            } => {
                let mut entry = typed_entry(id, parent_id, "branch_summary", timestamp);
                entry.from_id = Some(from_id);
                entry.summary = Some(summary);
                entry.details = details;
                entry.usage = usage;
                entry.from_hook = from_hook;
                entry
            }
            SessionRecord::Custom {
                id,
                parent_id,
                timestamp,
                custom_type,
                data,
            } => {
                let mut entry = typed_entry(id, parent_id, "custom", timestamp);
                entry.custom_type = Some(custom_type);
                entry.data = data;
                entry
            }
            SessionRecord::CustomMessage {
                id,
                parent_id,
                timestamp,
                custom_type,
                content,
                details,
                display,
            } => {
                let mut entry = typed_entry(id, parent_id, "custom_message", timestamp);
                entry.custom_type = Some(custom_type);
                entry.content = Some(content);
                entry.details = details;
                entry.display = Some(display);
                entry
            }
            SessionRecord::TodoSnapshot {
                id,
                parent_id,
                timestamp,
                state,
                execution_status,
                failure_reason,
            } => {
                let mut entry = typed_entry(id, parent_id, "todo_snapshot", timestamp);
                entry.todo_state = Some(state);
                entry.todo_execution_status = execution_status;
                entry.todo_failure_reason = failure_reason;
                entry
            }
            SessionRecord::Label {
                id,
                parent_id,
                timestamp,
                target_id,
                label,
            } => {
                let mut entry = typed_entry(id, parent_id, "label", timestamp);
                entry.target_id = Some(target_id);
                entry.label = label;
                entry
            }
            SessionRecord::Checkpoint {
                id,
                parent_id,
                timestamp,
                name,
                target_id,
            } => {
                let mut entry = typed_entry(id, parent_id, "checkpoint", timestamp);
                entry.name = Some(name);
                entry.target_id = Some(target_id);
                entry
            }
        };
        reject_duplicate_entry_id(&by_id, &entry.id, entries.len() + 2)?;
        let key = entry.id.clone();
        by_id.insert(key, entries.len());
        entries.push(entry);
    }
    let leaf_id = entries
        .iter()
        .rev()
        .find(|entry| entry.entry_type != "label" && entry.entry_type != "checkpoint")
        .map(|entry| entry.id.clone());
    let (children_by_parent, labels) = build_tree_indexes(&entries, &by_id);
    Ok(SessionTree {
        header,
        entries,
        active_leaf_id: leaf_id.clone(),
        leaf_id,
        skipped_idless,
        undecodable_messages: 0,
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
                    .and_then(|content| {
                        serde_json::from_value::<CustomMessageContent>(content).ok()
                    })
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
        && id.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
        && id
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_alphanumeric())
        && id
            .chars()
            .next_back()
            .is_some_and(|character| character.is_ascii_alphanumeric());
    if !valid {
        bail!(
            "Session id must be non-empty, contain only alphanumeric characters, '-', '_', and '.', and start and end with an alphanumeric character"
        );
    }
    Ok(())
}

#[must_use]
pub fn list_sessions_in(cwd: impl AsRef<Path>, session_dir: Option<&Path>) -> Vec<SessionInfo> {
    let cwd = absolute_path(cwd.as_ref());
    let directory = session_dir.map_or_else(|| default_session_dir(&cwd), absolute_path);
    let filter_cwd = session_dir.is_some();
    let Ok(capabilities) = CapabilityFs::new() else {
        return Vec::new();
    };
    let Ok(absolute) = AbsolutePath::parse(directory.clone()) else {
        return Vec::new();
    };
    let Ok(lease) = capabilities.open_root(
        CapabilityRootPurpose::SessionStore,
        absolute,
        CapabilityAccess::PrivateReadWrite,
        RootCreation::Existing,
    ) else {
        return Vec::new();
    };
    let Ok(mut read_dir) = lease.read_dir(Path::new("")) else {
        return Vec::new();
    };
    let mut sessions = Vec::new();
    while sessions.len() < MAX_SESSION_RECORDS {
        let Ok(Some(entry)) = read_dir.next() else {
            break;
        };
        if Path::new(entry.name())
            .extension()
            .is_none_or(|extension| extension != "jsonl")
        {
            continue;
        }
        let CapabilityEntry::File(file) = entry.into_entry() else {
            continue;
        };
        let modified = file
            .metadata()
            .ok()
            .and_then(|metadata| modified_from_ns(metadata.modified_ns));
        let path = file.display_path();
        let Ok(standard) = file.try_clone_std() else {
            continue;
        };
        if let Ok(session) = load_session_tree_from_file(standard, &path)
            .and_then(|tree| session_info_from_tree(path, tree))
            && (!filter_cwd || absolute_path(&session.cwd) == cwd)
        {
            sessions.push((session, modified));
        }
    }
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

/// Best-effort TTL cleanup of native session files.
///
/// Walks each root in `roots` (typically [`native_sessions_root`] and/or the
/// resolved session directory) and deletes every `*.jsonl` session file whose
/// last-modified time is older than `ttl`. Only the native pi session tree is
/// touched: foreign sources (codex/claude/grok) live in separate roots and are
/// never walked here. Deletion is scoped to regular files directly in a root,
/// one directory deep (per-cwd `--<encoded-cwd>--` directories), or two deep
/// (durable children under `children/<parent-id>/`). Symlinks and symlinked
/// directories are never followed, and non-`.jsonl` files (e.g. loop-scheduler
/// `.loops.json` sidecars) are left alone. A directory containing any symlink
/// (or other unsupported entry) is left entirely untouched — its sibling
/// files are not pruned from a partially validated snapshot, since acting on
/// an enumeration that silently dropped the link could not be trusted. After
/// deleting, now-empty subdirectories are removed best-effort.
///
/// Never pruned: files modified within [`SESSION_ACTIVE_GRACE`] of `now`
/// (the conservative active/locked-session guard — the native store has no
/// lock files), files listed in `skip` (the current session's file, whether
/// freshly started or resumed from an old mtime), directories listed in
/// `dir_skip` (the live run's session directory root and the parent of its
/// session file — never removed even when empty, since a just-started
/// recorder may not have flushed its first file yet), and files with a future
/// mtime or unreadable metadata.
///
/// I/O errors are swallowed so the call can never fail startup. Returns the
/// number of deleted session files.
#[must_use]
pub fn prune_expired_sessions(
    roots: &[PathBuf],
    now: SystemTime,
    ttl: Duration,
    skip: &[PathBuf],
    dir_skip: &[PathBuf],
) -> usize {
    let skip = skip
        .iter()
        .map(|path| absolute_path(path))
        .collect::<HashSet<_>>();
    let dir_skip = dir_skip
        .iter()
        .map(|path| absolute_path(path))
        .collect::<HashSet<_>>();
    let mut deleted = 0;
    let mut seen_roots = HashSet::new();
    for root in roots {
        let root = absolute_path(root);
        if !seen_roots.insert(root.clone()) {
            continue;
        }
        let Ok(lease) = open_session_store_lease(&root, RootCreation::Existing) else {
            continue;
        };
        deleted += prune_expired_in_lease(&lease, Path::new(""), 0, now, ttl, &skip, &dir_skip);
    }
    deleted
}

fn prune_expired_in_lease(
    lease: &crate::fs::CapabilityRootLease,
    relative: &Path,
    depth: usize,
    now: SystemTime,
    ttl: Duration,
    skip: &HashSet<PathBuf>,
    dir_skip: &HashSet<PathBuf>,
) -> usize {
    // Fail closed on symlink entries: a symlink (or any other unsupported
    // entry) in the directory produces a partially validated snapshot. The
    // prune never follows or deletes the link, and it does not prune sibling
    // files from a snapshot it could not fully validate — touch nothing in
    // this directory and skip its subtree.
    if lease.read_dir_names_strict(relative).is_err() {
        return 0;
    }
    let Ok(mut read_dir) = lease.read_dir(relative) else {
        return 0;
    };
    let mut deleted = 0;
    let mut subdirs = Vec::new();
    while let Ok(Some(entry)) = read_dir.next() {
        let child = relative.join(entry.name());
        match entry.into_entry() {
            CapabilityEntry::Directory(_) => subdirs.push(child),
            CapabilityEntry::File(file) => {
                let path = file.display_path();
                if path
                    .extension()
                    .is_none_or(|extension| extension != "jsonl")
                    || skip.contains(&path)
                {
                    continue;
                }
                let Ok(metadata) = file.metadata() else {
                    continue;
                };
                if session_metadata_expired(metadata.modified_ns, now, ttl)
                    && lease
                        .remove_file_expected(&child, metadata.identity)
                        .is_ok()
                {
                    deleted += 1;
                }
            }
        }
    }
    if depth < 2 {
        for subdir in subdirs {
            deleted += prune_expired_in_lease(lease, &subdir, depth + 1, now, ttl, skip, dir_skip);
            let display = lease.display_root().join(&subdir);
            if !dir_skip.contains(&display) {
                let _ = lease.remove_directory(&subdir);
            }
        }
    }
    deleted
}

fn session_metadata_expired(modified_ns: i128, now: SystemTime, ttl: Duration) -> bool {
    let Some(modified) = modified_from_ns(modified_ns) else {
        return false;
    };
    let floor = std::time::UNIX_EPOCH + Duration::from_secs(PRUNE_MTIME_FLOOR_SECS);
    if modified < floor {
        return false;
    }
    let Ok(age) = now.duration_since(modified) else {
        return false;
    };
    age >= SESSION_ACTIVE_GRACE && age > ttl
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
    let root = default_session_dir(cwd);
    let relative = validated_saved_session_relative(&root, path.as_ref())?;
    let lease = open_session_store_lease(&root, RootCreation::Existing)?;
    let file = lease.open_file(&relative)?;
    let identity = file.metadata()?.identity;
    lease
        .remove_file_expected(&relative, identity)
        .with_context(|| format!("deleting saved session {}", path.as_ref().display()))
}

/// Durably archive a native JSONL session and its recognized regular sidecars.
pub fn archive_saved_session(path: impl AsRef<Path>) -> Result<SessionArchiveOutcome> {
    let prepared = PreparedSessionResume::prepare_path(path.as_ref())?;
    let session_id = prepared.tree().header.id.clone();
    let cwd = prepared.target_cwd().to_path_buf();
    let original_path = prepared.path().to_path_buf();
    let source_dir = original_path
        .parent()
        .ok_or_else(|| anyhow!("session archive source has no parent directory"))?
        .to_path_buf();
    let primary_name = original_path
        .file_name()
        .ok_or_else(|| anyhow!("session archive source has no file name"))?
        .to_os_string();
    let agent_base = archive_agent_base(&source_dir)?;
    let source = open_session_store_lease(&source_dir, RootCreation::Existing)?;
    let child_relative = Path::new("children").join(&session_id);
    if source.entry_exists(&child_relative)? {
        bail!(
            "session {} has durable child state at {}; archive it before removing the parent",
            session_id,
            source.display_root().join(child_relative).display()
        );
    }
    let repo_digest = crate::memory::repo_namespace(&cwd);
    let archive_dir = agent_base.join("sessions-archived").join(&repo_digest);
    drop(prepared);

    let sources = open_archive_sources(&source, &primary_name)?;
    let archive = open_archive_lease(&agent_base, &repo_digest)?;
    let members = copy_archive_sources(&sources, &archive)?;
    if let Err(error) = archive.sync_directory(Path::new("")) {
        cleanup_archive_members(&archive, &members)?;
        return Err(error).context("syncing session archive directory");
    }

    let outcome = SessionArchiveOutcome {
        session_id,
        original_path,
        archive_dir,
        source,
        archive,
        members,
    };
    remove_archive_sources(&outcome, &sources)?;
    Ok(outcome)
}
pub fn preflight_archive_saved_session(path: impl AsRef<Path>) -> Result<()> {
    let prepared = PreparedSessionResume::prepare_path(path.as_ref())?;
    let source_dir = prepared
        .path()
        .parent()
        .ok_or_else(|| anyhow!("session archive source has no parent directory"))?
        .to_path_buf();
    archive_agent_base(&source_dir)?;
    let source = open_session_store_lease(&source_dir, RootCreation::Existing)?;
    let child_relative = Path::new("children").join(&prepared.tree().header.id);
    if source.entry_exists(&child_relative)? {
        bail!(
            "session {} has durable child state at {}; archive it before removing the parent",
            prepared.tree().header.id,
            source.display_root().join(child_relative).display()
        );
    }
    Ok(())
}

/// Restore an archived session to its exact original member names and bytes.
pub fn restore_archived_session(outcome: &SessionArchiveOutcome) -> Result<()> {
    let archives = open_archive_members(outcome)?;
    for member in &outcome.members {
        if outcome
            .source
            .entry_exists(Path::new(&member.original_name))?
        {
            bail!(
                "refusing to restore archived session over existing entry {}",
                outcome
                    .source
                    .display_root()
                    .join(&member.original_name)
                    .display()
            );
        }
    }

    let mut restored = Vec::with_capacity(outcome.members.len());
    for (member, archive_file) in outcome.members.iter().zip(&archives) {
        match copy_archive_file(archive_file, &outcome.source, &member.original_name) {
            Ok(file) => restored.push((member.original_name.clone(), file.identity())),
            Err(error) => {
                if let Err(cleanup) = cleanup_created_files(&outcome.source, &restored) {
                    return Err(cleanup).context("cleaning incomplete session restore");
                }
                return Err(error).context("restoring archived session");
            }
        }
    }
    if let Err(error) = outcome.source.sync_directory(Path::new("")) {
        if let Err(cleanup) = cleanup_created_files(&outcome.source, &restored) {
            return Err(cleanup).context("cleaning unsynced session restore");
        }
        return Err(error).context("syncing restored session directory");
    }
    drop(archives);

    for member in &outcome.members {
        if outcome
            .archive
            .remove_file_expected(
                Path::new(&member.archive_name),
                member.archive_metadata.identity,
            )
            .is_err()
        {
            return Ok(());
        }
    }
    let _ = outcome.archive.sync_directory(Path::new(""));
    Ok(())
}

struct ArchiveSource {
    name: OsString,
    metadata: FsMetadata,
    file: CapabilityFile,
}

fn archive_agent_base(source_dir: &Path) -> Result<PathBuf> {
    let sessions = source_dir
        .ancestors()
        .find(|ancestor| ancestor.file_name() == Some(OsStr::new("sessions")))
        .ok_or_else(|| {
            anyhow!(
                "custom session root {} has no derivable owning agent base",
                source_dir.display()
            )
        })?;
    sessions
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| anyhow!("session storage root has no owning agent base"))
}

fn open_archive_lease(agent_base: &Path, repo_digest: &str) -> Result<CapabilityRootLease> {
    let agent = open_session_store_lease(agent_base, RootCreation::Existing)?;
    let archives = agent.lease_directory(
        Path::new("sessions-archived"),
        CapabilityAccess::PrivateReadWrite,
        RootCreation::CreateDirectory,
    )?;
    agent.sync_directory(Path::new(""))?;
    let archive = archives.lease_directory(
        Path::new(repo_digest),
        CapabilityAccess::PrivateReadWrite,
        RootCreation::CreateDirectory,
    )?;
    archives.sync_directory(Path::new(""))?;
    Ok(archive)
}

fn open_archive_sources(
    source: &CapabilityRootLease,
    primary_name: &OsStr,
) -> Result<Vec<ArchiveSource>> {
    let primary_text = primary_name
        .to_str()
        .ok_or_else(|| anyhow!("native session file name is not UTF-8"))?;
    let primary = source.open_file(Path::new(primary_name))?;
    let primary_metadata = primary.metadata()?;
    let mut sources = vec![ArchiveSource {
        name: primary_name.to_os_string(),
        metadata: primary_metadata,
        file: primary,
    }];
    for name in source.open_directory(Path::new(""))?.read_dir_names()? {
        if name == primary_name || !is_archive_sidecar(primary_text, &name) {
            continue;
        }
        let file = match source.open_file(Path::new(&name)) {
            Ok(file) => file,
            Err(error)
                if crate::fs::is_link_blocked(&error)
                    || matches!(error, FsError::UnsupportedEntryType) =>
            {
                continue;
            }
            Err(error) => return Err(error).context("opening session archive sidecar"),
        };
        let metadata = file.metadata()?;
        sources.push(ArchiveSource {
            name,
            metadata,
            file,
        });
    }
    sources[1..].sort_by(|left, right| left.name.cmp(&right.name));
    Ok(sources)
}

pub(crate) fn is_archive_sidecar(primary_name: &str, candidate: &OsStr) -> bool {
    let Some(candidate) = candidate.to_str() else {
        return false;
    };
    if candidate == format!("{primary_name}.loops.json") {
        return true;
    }
    ["rewind-", "snapcompact-"].into_iter().any(|kind| {
        candidate
            .strip_prefix(&format!("{primary_name}.{kind}"))
            .is_some_and(|suffix| suffix.len() > ".jsonl".len() && suffix.ends_with(".jsonl"))
    })
}

fn copy_archive_sources(
    sources: &[ArchiveSource],
    archive: &CapabilityRootLease,
) -> Result<Vec<SessionArchiveMember>> {
    let mut members = Vec::with_capacity(sources.len());
    for source in sources {
        match copy_archive_source(source, archive) {
            Ok(member) => members.push(member),
            Err(error) => {
                cleanup_archive_members(archive, &members)?;
                return Err(error);
            }
        }
    }
    Ok(members)
}

fn copy_archive_source(
    source: &ArchiveSource,
    archive: &CapabilityRootLease,
) -> Result<SessionArchiveMember> {
    for attempt in 0..=1000 {
        let archive_name = collision_archive_name(&source.name, attempt);
        match archive.create_new(Path::new(&archive_name), FileMode::Private) {
            Ok(destination) => {
                let archive_file = copy_into_created(&source.file, destination)?;
                let current = match source.file.metadata() {
                    Ok(current) => current,
                    Err(error) => {
                        archive.remove_file_expected(
                            Path::new(&archive_name),
                            archive_file.identity(),
                        )?;
                        return Err(error).context("revalidating session archive source");
                    }
                };
                if current.identity != source.metadata.identity
                    || current.len != source.metadata.len
                    || current.modified_ns != source.metadata.modified_ns
                {
                    archive
                        .remove_file_expected(Path::new(&archive_name), archive_file.identity())?;
                    bail!("session archive source changed while it was copied");
                }
                let archive_metadata = match archive_file.metadata() {
                    Ok(metadata) => metadata,
                    Err(error) => {
                        archive.remove_file_expected(
                            Path::new(&archive_name),
                            archive_file.identity(),
                        )?;
                        return Err(error).context("reading session archive metadata");
                    }
                };
                return Ok(SessionArchiveMember {
                    original_name: source.name.clone(),
                    archive_name,
                    archive_metadata,
                });
            }
            Err(FsError::System {
                kind: io::ErrorKind::AlreadyExists,
                ..
            }) => {}
            Err(error) => return Err(error).context("creating session archive member"),
        }
    }
    bail!("session archive collision limit exceeded")
}

fn collision_archive_name(original: &OsStr, attempt: usize) -> OsString {
    let mut name = original.to_os_string();
    if attempt > 0 {
        name.push(format!(".{attempt}"));
    }
    name
}

fn copy_archive_file(
    source: &CapabilityFile,
    destination: &CapabilityRootLease,
    name: &OsStr,
) -> Result<CapabilityFile> {
    let created = destination.create_new(Path::new(name), FileMode::Private)?;
    copy_into_created(source, created)
}

fn copy_into_created(
    source: &CapabilityFile,
    mut destination: crate::fs::CapabilityWritableFile,
) -> Result<CapabilityFile> {
    let mut reader = source
        .try_clone_std()
        .context("cloning session archive member")?;
    reader
        .seek(SeekFrom::Start(0))
        .context("seeking session archive member")?;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = reader
            .read(&mut buffer)
            .context("reading session archive member")?;
        if count == 0 {
            break;
        }
        destination.write_all(&buffer[..count])?;
    }
    destination.finish().map_err(anyhow::Error::new)
}

fn cleanup_archive_members(
    archive: &CapabilityRootLease,
    members: &[SessionArchiveMember],
) -> Result<()> {
    let files = members
        .iter()
        .map(|member| {
            (
                member.archive_name.clone(),
                member.archive_metadata.identity,
            )
        })
        .collect::<Vec<_>>();
    cleanup_created_files(archive, &files)
}

fn cleanup_created_files(
    directory: &CapabilityRootLease,
    files: &[(OsString, FileIdentity)],
) -> Result<()> {
    for (name, identity) in files.iter().rev() {
        directory.remove_file_expected(Path::new(name), *identity)?;
    }
    directory.sync_directory(Path::new(""))?;
    Ok(())
}

fn remove_archive_sources(
    outcome: &SessionArchiveOutcome,
    sources: &[ArchiveSource],
) -> Result<()> {
    for source in sources {
        let current = source.file.metadata()?;
        if current.identity != source.metadata.identity
            || current.len != source.metadata.len
            || current.modified_ns != source.metadata.modified_ns
        {
            cleanup_archive_members(&outcome.archive, &outcome.members)?;
            bail!("session archive source changed before removal");
        }
    }
    let mut removed = Vec::with_capacity(sources.len());
    let removal_order = (1..sources.len()).chain(std::iter::once(0));
    for index in removal_order {
        let source = &sources[index];
        if let Err(error) = outcome
            .source
            .remove_file_expected(Path::new(&source.name), source.metadata.identity)
        {
            rollback_removed_archive_sources(outcome, &removed)?;
            cleanup_archive_members(&outcome.archive, &outcome.members)?;
            return Err(error).context("removing archived session source");
        }
        removed.push(index);
    }
    if let Err(error) = outcome.source.sync_directory(Path::new("")) {
        rollback_removed_archive_sources(outcome, &removed)?;
        cleanup_archive_members(&outcome.archive, &outcome.members)?;
        return Err(error).context("syncing archived session source directory");
    }
    Ok(())
}

fn rollback_removed_archive_sources(
    outcome: &SessionArchiveOutcome,
    removed: &[usize],
) -> Result<()> {
    for index in removed.iter().rev().copied() {
        let member = &outcome.members[index];
        let archived = outcome.archive.open_file(Path::new(&member.archive_name))?;
        copy_archive_file(&archived, &outcome.source, &member.original_name)
            .context("rolling back incomplete session archive")?;
    }
    outcome
        .source
        .sync_directory(Path::new(""))
        .context("syncing rolled-back session archive source directory")
}

fn open_archive_members(outcome: &SessionArchiveOutcome) -> Result<Vec<CapabilityFile>> {
    outcome
        .members
        .iter()
        .map(|member| {
            let file = outcome.archive.open_file(Path::new(&member.archive_name))?;
            if file.metadata()? != member.archive_metadata {
                bail!("session archive member identity changed");
            }
            Ok(file)
        })
        .collect()
}

pub fn validated_saved_session_path(root: &Path, path: &Path) -> Result<PathBuf> {
    let relative = validated_saved_session_relative(root, path)?;
    Ok(root.join(relative))
}

fn validated_saved_session_relative(root: &Path, path: &Path) -> Result<PathBuf> {
    if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl") {
        bail!(
            "refusing saved-session mutation outside a .jsonl file: {}",
            path.display()
        );
    }
    let candidate = if path.is_absolute() {
        path.to_path_buf()
    } else {
        absolute_path(path)
    };
    let relative = candidate.strip_prefix(root).map_err(|_| {
        anyhow!(
            "refusing saved-session mutation outside session root {}: {}",
            root.display(),
            candidate.display()
        )
    })?;
    if relative
        .parent()
        .is_some_and(|parent| !parent.as_os_str().is_empty())
        || relative.file_name().is_none()
    {
        bail!(
            "refusing saved-session mutation outside session root {}: {}",
            root.display(),
            candidate.display()
        );
    }
    let capabilities = CapabilityFs::new().context("initializing session filesystem capability")?;
    let lease = capabilities.open_root(
        CapabilityRootPurpose::SessionStore,
        AbsolutePath::parse(root.to_path_buf())
            .map_err(|_| anyhow!("invalid session root {}", root.display()))?,
        CapabilityAccess::PrivateReadWrite,
        RootCreation::Existing,
    )?;
    lease.open_file(relative)?;
    Ok(relative.to_path_buf())
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

/// Validate a `/checkpoint <name>` marker name.
///
/// A checkpoint name is a single whitespace-free word (the CLI splits slash
/// arguments on whitespace, so anything else would be unreachable) and must
/// not look like an entry index, because `/rewind 5` always resolves to an
/// index and never to a checkpoint.
fn normalize_checkpoint_name(name: &str) -> Result<String> {
    let normalized = name.trim();
    anyhow::ensure!(!normalized.is_empty(), "checkpoint name must not be empty");
    anyhow::ensure!(
        !normalized.chars().any(char::is_whitespace),
        "checkpoint name must be a single word (no whitespace)"
    );
    anyhow::ensure!(
        normalized.parse::<usize>().is_err(),
        "checkpoint name must not be a plain number (use /rewind <index> for entry indices)"
    );
    Ok(normalized.to_owned())
}

/// Write the truncated record tail to a `.rewind-<timestamp>.jsonl` sidecar
/// next to the session file (same directory, same record serialization, no
/// header). Writes with `create_new` and bumps a numeric suffix on collision
/// so an archive can never overwrite an earlier one. fsyncs before returning.
fn archive_rewind_tail(state: &mut RecorderState, tail: &[u8]) -> Result<PathBuf> {
    ensure_session_parent(state)?;
    let lease = state
        .lease
        .as_ref()
        .ok_or_else(|| anyhow!("session directory capability is unavailable"))?;
    let file_name = state
        .path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("session");
    let timestamp = iso_now().replace([':', '.'], "-");
    for attempt in 0..=1000 {
        let name = if attempt == 0 {
            format!("{file_name}.rewind-{timestamp}.jsonl")
        } else {
            format!("{file_name}.rewind-{timestamp}-{attempt}.jsonl")
        };
        match lease.create_new(Path::new(&name), crate::fs::FileMode::Private) {
            Ok(mut archive) => {
                archive.write_all(tail)?;
                archive.finish()?;
                return Ok(lease.display_root().join(name));
            }
            Err(crate::fs::FsError::System {
                kind: std::io::ErrorKind::AlreadyExists,
                ..

            }) => {}
            Err(error) => return Err(error).context("creating rewind archive"),
        }
    }
    bail!("rewind archive collision limit exceeded")
}
fn fork_tree_and_raw_records_from_state(state: &RecorderState) -> Result<(SessionTree, Vec<Value>)> {
    let mut values = if state.flushed {
        let file = state
            .file
            .as_ref()
            .ok_or_else(|| anyhow!("session append writer is unavailable for live fork capture"))?;
        read_session_values_from_file(
            file.try_clone()
                .context("cloning attached session handle for fork capture")?,
            &state.path,
        )?
    } else {
        Vec::new()
    };
    if state.flushed {
        values.extend(state.pending.iter().cloned());
    } else {
        values.clone_from(&state.pending);
    }
    let mut tree = session_tree_from_values(&state.path, &values)?;
    tree.active_leaf_id.clone_from(&state.active_leaf_id);
    let raw_records = values.into_iter().skip(1).collect();
    Ok((tree, raw_records))
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
                file.try_clone()
                    .context("cloning attached session handle for tree load")?,
                &state.path,
            )?
        } else {
            bail!("session append writer is unavailable for tree projection");
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
            let mut combined = session_tree_from_values(&state.path, &values)?;
            combined.skipped_idless = combined
                .skipped_idless
                .saturating_add(persisted.skipped_idless);
            combined.undecodable_messages = combined
                .undecodable_messages
                .saturating_add(persisted.undecodable_messages);
            combined
        }
    } else {
        session_tree_from_values(&state.path, &state.pending)?
    };
    tree.active_leaf_id.clone_from(&state.active_leaf_id);
    Ok(tree)
}
fn is_already_exists(error: &anyhow::Error) -> bool {
    error.chain().any(|source| {
        source.downcast_ref::<FsError>().is_some_and(|error| {
            matches!(
                error,
                FsError::System {
                    kind: io::ErrorKind::AlreadyExists,
                    ..
                }
            )
        })
    })
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
                    format!("rolling back durable session append after error: {error:#}")
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

/// Recreate the recorder's session directory if a prune (or manual cleanup)
/// removed it while the recorder was still holding its first write in memory.
/// The auto-id startup path keeps the file unwritten until the first flush, so
/// the directory can legitimately be empty on disk; this makes the first flush
/// succeed regardless. No-op when the parent already exists.
fn ensure_session_parent(state: &mut RecorderState) -> Result<()> {
    if state.lease.is_none() {
        let parent = state
            .path
            .parent()
            .ok_or_else(|| anyhow!("session path {} has no parent", state.path.display()))?;
        state.lease = Some(open_session_store_lease(
            parent,
            RootCreation::CreateDirectory,
        )?);
    }
    Ok(())
}

fn open_session_store_lease(
    directory: &Path,
    creation: RootCreation,
) -> Result<crate::fs::CapabilityRootLease> {
    let capabilities = CapabilityFs::new().context("initializing session filesystem capability")?;
    capabilities
        .open_root(
            CapabilityRootPurpose::SessionStore,
            AbsolutePath::parse(directory.to_path_buf())
                .map_err(|_| anyhow!("invalid session directory {}", directory.display()))?,
            CapabilityAccess::PrivateReadWrite,
            creation,
        )
        .map_err(anyhow::Error::from)
}

fn validate_append_writer(
    state: &mut RecorderState,
) -> Result<(String, PathBuf, crate::SessionFileIdentity)> {
    let expected_identity = state
        .file_identity
        .ok_or_else(|| anyhow!("session file identity is unavailable"))?;
    let file = state
        .file
        .as_mut()
        .ok_or_else(|| anyhow!("session append writer is unavailable"))?;
    file.metadata()
        .context("reading session append writer metadata")?;
    if crate::SessionFileIdentity::from_file(file)? != expected_identity {
        bail!("session append writer identity changed");
    }
    let lease = state
        .lease
        .as_ref()
        .ok_or_else(|| anyhow!("session directory capability is unavailable"))?;
    lease.validate_live_root()?;
    let relative = state
        .path
        .strip_prefix(lease.display_root())
        .context("session path is outside retained session root")?;
    let opened = lease
        .open_file(relative)
        .context("revalidating session append path")?;
    if opened.public_identity() != expected_identity {
        bail!("session append path identity changed");
    }
    file.seek(SeekFrom::End(0))
        .context("seeking session append writer")?;
    Ok((state.id.clone(), state.path.clone(), expected_identity))
}

fn persist_durable(state: &mut RecorderState) -> Result<()> {
    if !state.flushed {
        create_session_file(state)?;
        return Ok(());
    }
    if !state.pending.is_empty()
        && let Some(file) = state.file.as_mut()
    {
        write_records_durable(file, &state.pending, state.requires_separator)?;
        state.pending.clear();
        state.requires_separator = false;
    }
    Ok(())
}

fn create_session_file(state: &mut RecorderState) -> Result<()> {
    ensure_session_parent(state)?;
    let name = state
        .path
        .file_name()
        .ok_or_else(|| anyhow!("session path {} has no file name", state.path.display()))?;
    let lease = state
        .lease
        .as_ref()
        .ok_or_else(|| anyhow!("session directory capability is unavailable"))?;
    let mut created = lease.create_new(Path::new(name), crate::fs::FileMode::Private)?;
    let serialized = serialize_records(&state.pending, false)?;
    created.write_all(&serialized)?;
    let opened = created.finish()?;
    state.file_identity = Some(opened.public_identity());
    state.file = Some(
        opened
            .try_clone_std()
            .context("cloning new session handle")?,
    );
    state.flushed = true;
    state.pending.clear();
    state.requires_separator = false;
    Ok(())
}

fn persist(state: &mut RecorderState) -> Result<()> {
    if !state.durable && !state.has_assistant {
        return Ok(());
    }
    if !state.flushed {
        create_session_file(state)?;
        return Ok(());
    }
    if !state.pending.is_empty()
        && let Some(file) = state.file.as_mut()
    {
        if state.durable {
            write_records_durable(file, &state.pending, state.requires_separator)?;
        } else {
            write_records(file, &state.pending, state.requires_separator)?;
        }
        state.pending.clear();
        state.requires_separator = false;
    }
    Ok(())
}

fn serialize_records(records: &[Value], requires_separator: bool) -> Result<Vec<u8>> {
    let mut serialized = Vec::new();
    if requires_separator && !records.is_empty() {
        serialized.push(b'\n');
    }
    for record in records {
        serde_json::to_writer(&mut serialized, record).context("serializing session record")?;
        serialized.push(b'\n');
    }
    Ok(serialized)
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

fn write_records(file: &mut File, records: &[Value], requires_separator: bool) -> Result<()> {
    let serialized = serialize_records(records, requires_separator)?;
    append_serialized_records(file, &serialized).context("writing session record")
}

fn write_records_durable(
    file: &mut File,
    records: &[Value],
    requires_separator: bool,
) -> Result<()> {
    let serialized = serialize_records(records, requires_separator)?;
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
                format!(
                    "{write_error}; rolling back partial session append failed: {rollback_error}"
                ),
            ));
        }
        return Err(write_error);
    }
    Ok(())
}

fn append_serialized_records<S: RecordAppendSink>(
    sink: &mut S,
    serialized: &[u8],
) -> io::Result<()> {
    let previous_len = sink.append_len()?;
    let write_result = sink.write_all(serialized).and_then(|()| sink.flush());
    if let Err(write_error) = write_result {
        if let Err(rollback_error) = sink
            .truncate_append(previous_len)
            .and_then(|()| sink.sync_rollback())
        {
            return Err(io::Error::new(
                write_error.kind(),
                format!(
                    "{write_error}; rolling back partial session append failed: {rollback_error}"
                ),
            ));
        }
        return Err(write_error);
    }
    Ok(())
}

fn read_session_info(path: &Path) -> Result<SessionInfo> {
    session_info_from_tree(path.to_path_buf(), load_session_tree(path)?)
}

fn session_info_from_tree(path: PathBuf, tree: SessionTree) -> Result<SessionInfo> {
    let session_name = tree.session_name();
    let message_text = tree
        .entries
        .iter()
        .filter_map(|entry| entry.message.as_ref())
        .map(message_search_text)
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>();
    Ok(SessionInfo {
        path,
        id: tree.header.id,
        cwd: tree.header.cwd,
        timestamp: tree.header.timestamp,
        name: session_name,
        messages: tree
            .entries
            .iter()
            .filter(|entry| entry.entry_type == "message")
            .count(),
        first_message: message_text
            .first()
            .cloned()
            .unwrap_or_else(|| "(no messages)".to_owned()),
        all_messages_text: message_text.join("\n"),
    })
}

fn modified_from_ns(modified_ns: i128) -> Option<SystemTime> {
    let nanos = u64::try_from(modified_ns).ok()?;
    Some(std::time::UNIX_EPOCH + Duration::from_nanos(nanos))
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

pub(crate) fn append_entry_message(messages: &mut Vec<Message>, entry: &SessionEntry) {
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
    use std::fs;
    fn retained_session_lease(
        directory: &Path,
        creation: crate::fs::RootCreation,
    ) -> crate::fs::CapabilityRootLease {
        let capabilities = crate::fs::CapabilityFs::new().expect("capability filesystem");
        capabilities
            .open_root(
                crate::fs::CapabilityRootPurpose::SessionStore,
                pi_sandbox::AbsolutePath::parse(directory.to_path_buf())
                    .expect("absolute session root"),
                crate::fs::CapabilityAccess::PrivateReadWrite,
                creation,
            )
            .expect("retained session lease")
    }

    fn recorder_in_retained_directory(directory: &Path) -> SessionRecorder {
        let lease = retained_session_lease(directory, crate::fs::RootCreation::CreateDirectory);
        let id = Uuid::now_v7().to_string();
        let timestamp = iso_now();
        let path = new_session_path(directory, &timestamp, &id, false);
        SessionRecorder {
            inner: Arc::new(Mutex::new(RecorderState {
                path,
                id: id.clone(),
                file_identity: None,
                timestamp: timestamp.clone(),
                cwd: directory.to_path_buf(),
                parent_session: None,
                last_id: None,
                active_leaf_id: None,
                revision: 0,
                used_ids: HashSet::new(),
                pending: vec![json!({
                    "type": "session",
                    "version": CURRENT_SESSION_VERSION,
                    "id": id,
                    "timestamp": timestamp,
                    "cwd": directory,
                    "parentSession": null,
                })],
                requires_separator: false,
                file: None,
                lease: Some(lease),
                flushed: false,
                has_assistant: false,
                session_name: None,
                durable: false,
            })),
        }
    }
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
        fs::write(
            &target,
            native_session_body(directory.path(), "target", "target"),
        )
        .expect("write target");
        symlink(&target, &link).expect("create symlink");

        let lease = retained_session_lease(directory.path(), crate::fs::RootCreation::Existing);
        let opened = match lease.open_read_write_file(Path::new("link.jsonl")) {
            Err(error) => error,
            Ok(_) => panic!("final symlink must be rejected"),
        };
        assert!(matches!(
            opened,
            crate::FsError::UnsupportedEntryType | crate::FsError::System { .. }
        ));
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
        assert_eq!(
            fs::read_to_string(&malformed).expect("read malformed"),
            malformed_body
        );

        let oversized = directory.path().join("oversized.jsonl");
        let file = File::create(&oversized).expect("create oversized");
        file.set_len(MAX_SESSION_FILE_BYTES + 1)
            .expect("extend oversized");
        let error = PreparedSessionResume::prepare_path(&oversized)
            .expect_err("oversized session must fail");
        assert!(format!("{error:#}").contains("64 MiB file safety limit"));
        assert_eq!(
            fs::metadata(&oversized).expect("oversized metadata").len(),
            MAX_SESSION_FILE_BYTES + 1
        );
    }

    #[test]
    fn prepared_resume_writer_activation_fails_closed_after_path_replacement() {
        let directory = tempfile::tempdir().expect("directory");
        let path = directory.path().join("session.jsonl");
        let held = directory.path().join("held.jsonl");
        let held_body = native_session_body(directory.path(), "held", "held");
        let replacement_body = native_session_body(directory.path(), "replacement", "replacement");
        fs::write(&path, &held_body).expect("write held session");

        let prepared = PreparedSessionResume::prepare_path(&path).expect("prepare held session");
        fs::rename(&path, &held).expect("retain opened inode under another name");
        fs::write(&path, &replacement_body).expect("write replacement path");

        let recorder = prepared.into_recorder().expect("build retained recorder");
        assert!(recorder.activate_writer().is_err());
        assert_eq!(fs::read_to_string(&held).expect("read held"), held_body);
        assert_eq!(
            fs::read_to_string(&path).expect("read replacement"),
            replacement_body
        );
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
        file.set_len(MAX_SESSION_FILE_BYTES + 1)
            .expect("extend oversized session");
        let error = load_session_tree(&path).expect_err("oversized file must fail");
        assert!(error.to_string().contains("64 MiB file safety limit"));
    }

    #[test]
    fn session_loader_rejects_line_over_safety_limit_with_context() {
        let directory = tempfile::tempdir().expect("directory");
        let path = directory.path().join("oversized-line.jsonl");
        let mut file = File::create(&path).expect("create session");
        writeln!(file, "{}", session_header_line(directory.path())).expect("write header");
        file.write_all(&vec![b'x'; MAX_SESSION_LINE_BYTES + 1])
            .expect("write oversized line");
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
        records.extend((0..MAX_SESSION_RECORDS).map(|index| {
            json!({
                "type": "custom",
                "id": format!("entry-{index}"),
                "parentId": null,
                "timestamp": "2026-01-01T00:00:00.000Z",
                "customType": "state",
            })
        }));
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
        assert!(
            error
                .to_string()
                .contains("100000 reconstructed message safety limit")
        );
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
        recorder
            .record_message(&Message::user_text("second", 0))
            .expect("append message");
        recorder.close().expect("close recorder");
        assert_eq!(
            message_texts(
                &load_session_tree(&path)
                    .expect("reload")
                    .build_context(None)
            ),
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
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "injected partial append",
                ));
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
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "injected flush failure",
                ));
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
        let error =
            append_serialized_records(&mut sink, b"{\"type\":\"message\",\"id\":\"partial\"}\n")
                .expect_err("partial append must fail");
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(sink.bytes, original);
        for line in sink
            .bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
        {
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
                file_identity: None,
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
                requires_separator: false,
                file: None,
                lease: None,
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
    fn discard_persisted_removes_only_the_recorders_exact_file_identity() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let recorder = start_session_in(
            workspace.path(),
            None,
            None,
            Some(sessions.path()),
            None,
            None,
        )
        .expect("recorder");
        recorder.persist_now_durable().expect("persist recorder");
        let path = recorder.path();
        let identity = recorder.file_identity().expect("file identity");
        assert!(path.is_file());
        let live_identity = crate::SessionFileIdentity::from_file(
            &std::fs::File::open(&path).expect("open persisted session"),
        )
        .expect("persisted session identity");
        assert_eq!(
            Some(identity),
            Some(live_identity),
            "recorder tracks the identity of the live session file"
        );

        recorder.discard_persisted().expect("discard recorder");
        assert!(!path.exists());
        assert!(recorder.file_identity().is_none());
        assert!(
            recorder.discard_persisted().is_err(),
            "an already-discarded recorder cannot remove a replacement"
        );
        let replacement = start_session_in(
            workspace.path(),
            None,
            None,
            Some(sessions.path()),
            None,
            None,
        )
        .expect("replacement recorder");
        replacement
            .persist_now_durable()
            .expect("persist replacement");
        // Inodes freed by discard_persisted are routinely reused for the next
        // file created in the same directory, so a fresh stat (not a
        // cross-lifetime inequality) proves which file the replacement tracks.
        let replacement_identity = crate::SessionFileIdentity::from_file(
            &std::fs::File::open(replacement.path()).expect("open replacement session"),
        )
        .expect("replacement session identity");
        assert_eq!(
            replacement.file_identity(),
            Some(replacement_identity),
            "replacement recorder tracks its own new session file"
        );
        assert!(replacement.path().is_file());
    }

    #[test]
    fn persist_now_writes_header_without_assistant() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let recorder = start_session_in(
            workspace.path(),
            None,
            None,
            Some(sessions.path()),
            None,
            Some(Path::new("parent.jsonl")),
        )
        .expect("start session");
        assert!(!recorder.path().exists());
        recorder.persist_now().expect("persist header");
        let tree = load_session_tree(recorder.path()).expect("load header-only session");
        assert_eq!(tree.header.parent_session.as_deref(), Some("parent.jsonl"));
        assert!(tree.entries.is_empty());
        recorder.close().expect("close recorder");
    }

    #[test]
    fn tree_projection_fails_closed_without_retained_writer() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let recorder = start_session_in(
            workspace.path(),
            None,
            None,
            Some(sessions.path()),
            None,
            None,
        )
        .expect("start session");
        recorder
            .record_message(&Message::user_text("durable", 0))
            .expect("record");
        recorder.persist_now().expect("persist");
        let path = recorder.path().to_path_buf();
        recorder.close().expect("close writer");
        let error = recorder
            .tree()
            .expect_err("closed writer must not reopen the session path");
        assert!(
            error
                .to_string()
                .contains("session append writer is unavailable for tree projection"),
            "{error}"
        );
        let reopened = load_session_tree(&path).expect("path load still works");
        assert_eq!(
            message_texts(&reopened.build_context(None)),
            vec!["durable".to_owned()]
        );

        std::fs::write(
            &path,
            format!(
                "{{\"type\":\"session\",\"version\":3,\"id\":\"replacement\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"cwd\":{}}}\n{{\"type\":\"message\",\"id\":\"replacement\",\"parentId\":null,\"timestamp\":\"2026-01-01T00:00:00Z\",\"message\":{}}}\n",
                serde_json::to_string(workspace.path()).expect("cwd json"),
                serde_json::to_string(&Message::user_text("path-replacement", 0)).expect("message json"),
            ),
        )
        .expect("replace detached path");
        let error = recorder
            .tree()
            .expect_err("detached writer must not load the replacement inode");
        assert!(
            error
                .to_string()
                .contains("session append writer is unavailable for tree projection"),
            "{error}"
        );
        let replaced = load_session_tree(&path).expect("path now holds replacement");
        assert_eq!(
            message_texts(&reopened.build_context(None)),
            vec!["durable".to_owned()]
        );
        assert_eq!(
            message_texts(&replaced.build_context(None)),
            vec!["path-replacement".to_owned()]
        );
    }

    #[test]
    fn unflushed_empty_session_projects_pending_without_writer() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let recorder = start_session_in(
            workspace.path(),
            None,
            None,
            Some(sessions.path()),
            None,
            None,
        )
        .expect("start session");
        let tree = recorder
            .tree()
            .expect("unflushed empty session projects pending header");
        assert!(tree.entries.is_empty());
        assert!(!recorder.path().exists());
    }

    /// Canonical tuple of every `SessionTree` field that materially decides
    /// reload semantics; used to prove the typed fast path and the lenient
    /// Value path build identical trees regardless of HashMap iteration order.
    fn tree_equivalence_key(
        tree: &SessionTree,
    ) -> (
        String,
        String,
        Option<String>,
        Option<String>,
        usize,
        usize,
        Vec<(Option<String>, Vec<usize>)>,
        Vec<(String, (String, String))>,
    ) {
        let mut children: Vec<_> = tree
            .children_by_parent
            .iter()
            .map(|(parent, indexes)| (parent.clone(), indexes.clone()))
            .collect();
        children.sort_by(|left, right| left.0.cmp(&right.0));
        let mut labels: Vec<_> = tree
            .labels
            .iter()
            .map(|(target, resolved)| {
                (
                    target.clone(),
                    (resolved.label.clone(), resolved.timestamp.clone()),
                )
            })
            .collect();
        labels.sort_by(|left, right| left.0.cmp(&right.0));
        (
            serde_json::to_string(&tree.header).expect("serialize header"),
            serde_json::to_string(&tree.entries).expect("serialize entries"),
            tree.leaf_id.clone(),
            tree.active_leaf_id.clone(),
            tree.skipped_idless,
            tree.undecodable_messages,
            children,
            labels,
        )
    }

    #[test]
    fn typed_fast_path_builds_identical_tree_to_lenient_value_path() {
        let directory = tempfile::tempdir().expect("directory");
        let recorder = start_session_in(
            directory.path(),
            Some(&Model::default()),
            Some("medium"),
            Some(directory.path()),
            Some("typed-equivalence"),
            None,
        )
        .expect("start session");
        let user_id = recorder
            .record_message(&Message::user_text("first question", 1))
            .expect("user message");
        let mut assistant = pi_ai::AssistantMessage::pending(&Model::default());
        assistant.content = vec![ContentBlock::text(
            "first answer with **markdown** and `code`",
        )];
        recorder
            .record_message(&Message::Assistant(assistant))
            .expect("assistant message");
        recorder
            .record_custom_entry("extension.state", Some(json!({"cursor": 7})))
            .expect("custom entry");
        recorder
            .record_custom_message(&CustomMessage {
                custom_type: "extension.notice".to_owned(),
                content: CustomMessageContent::Text("remember".to_owned()),
                display: false,
                details: Some(json!({"origin": "test"})),
                timestamp: 3,
            })
            .expect("custom message");
        recorder
            .record_todo_snapshot(&crate::TodoState {
                phases: Vec::new(),
                storage: crate::TodoStorage::Session,
            })
            .expect("todo snapshot");
        recorder
            .record_session_name("typed-equivalence-name")
            .expect("session name");
        recorder
            .record_compaction(
                "compacted summary",
                Some(&user_id),
                12,
                &[Message::user_text("kept tail", 4)],
            )
            .expect("compaction");
        let leaf = recorder.last_entry_id().expect("leaf");
        recorder
            .record_label(&leaf, Some("checkpoint"))
            .expect("label");
        recorder.record_checkpoint("mark").expect("checkpoint");
        recorder
            .branch_with_summary(Some(&leaf), "branch summary")
            .expect("branch summary");
        recorder.persist_now().expect("persist");
        recorder.close().expect("close");

        let path = recorder.path();
        let bytes = fs::read(&path).expect("read session");
        let mut values = Vec::new();
        let mut records = Vec::new();
        for line in bytes
            .split(|b| *b == b'\n')
            .filter(|l| !l.is_empty() && !l.iter().all(|b| b.is_ascii_whitespace()))
        {
            values.push(serde_json::from_slice::<Value>(line).expect("value row"));
            records.push(serde_json::from_slice::<SessionRecord>(line).expect("typed row"));
        }
        let header = values[0].clone();
        let slow = session_tree_from_values(&path, &values).expect("lenient tree");
        let fast = session_tree_from_records(&path, &header, records.into_iter().skip(1).collect())
            .expect("typed tree");

        assert_eq!(tree_equivalence_key(&slow), tree_equivalence_key(&fast));
        // Non-vacuous: both paths decode every message record observed.
        assert_eq!(
            slow.entries
                .iter()
                .filter(|e| e.entry_type == "message")
                .count(),
            2
        );
        assert_eq!(slow.entries.len(), fast.entries.len());

        let slow_ctx = slow.build_context(None);
        let fast_ctx = fast.build_context(None);
        assert_eq!(
            serde_json::to_value(&slow_ctx.messages).expect("slow messages"),
            serde_json::to_value(&fast_ctx.messages).expect("fast messages")
        );
        assert_eq!(slow_ctx.thinking_level, fast_ctx.thinking_level);
        assert_eq!(slow_ctx.provider, fast_ctx.provider);
        assert_eq!(slow_ctx.model_id, fast_ctx.model_id);
    }

    #[test]
    fn load_uses_single_pass_typed_parse_for_writer_generated_sessions() {
        let directory = tempfile::tempdir().expect("directory");
        let recorder = start_session_in(
            directory.path(),
            Some(&Model::default()),
            Some("medium"),
            Some(directory.path()),
            Some("typed-path"),
            None,
        )
        .expect("start session");
        for i in 0..50 {
            recorder
                .record_message(&Message::user_text(format!("question {i}"), i))
                .expect("user");
            let mut assistant = pi_ai::AssistantMessage::pending(&Model::default());
            assistant.content = vec![ContentBlock::text(format!(
                "answer {i} with \u{4e2d}\u{6587} text and `code`"
            ))];
            recorder
                .record_message(&Message::Assistant(assistant))
                .expect("assistant");
        }
        recorder.persist_now().expect("persist");
        recorder.close().expect("close");

        let typed_before = TYPED_LOAD_PATH_HITS.with(|hits| hits.get());
        let lenient_before = LENIENT_LOAD_PATH_HITS.with(|hits| hits.get());
        let tree = load_session_tree(&recorder.path()).expect("load");
        // The full writer-generated session must go through the single-pass
        // typed path (and never the lenient slow path).
        assert_eq!(
            TYPED_LOAD_PATH_HITS.with(|hits| hits.get()),
            typed_before + 1
        );
        assert_eq!(
            LENIENT_LOAD_PATH_HITS.with(|hits| hits.get()),
            lenient_before
        );
        // header + model_change + thinking_level_change + 100 messages
        assert_eq!(tree.entries.len(), 102);
    }

    #[test]
    fn load_falls_back_to_lenient_value_parse_for_unknown_record_shapes() {
        let directory = tempfile::tempdir().expect("directory");
        let path = directory.path().join("unknown.jsonl");
        fs::write(
            &path,
            format!(
                "{}\n{}\n{}\n",
                session_header_line(directory.path()),
                serde_json::to_string(&json!({
                    "type": "message",
                    "id": "first",
                    "parentId": null,
                    "timestamp": "2026-01-01T00:00:01.000Z",
                    "message": Message::user_text("first", 0),
                }))
                .expect("message row"),
                serde_json::to_string(&json!({
                    "type": "telemetry_snapshot",
                    "id": "telemetry",
                    "parentId": null,
                    "timestamp": "2026-01-01T00:00:02.000Z",
                    "samples": [1, 2, 3],
                }))
                .expect("unknown row")
            ),
        )
        .expect("write session");

        let typed_before = TYPED_LOAD_PATH_HITS.with(|hits| hits.get());
        let lenient_before = LENIENT_LOAD_PATH_HITS.with(|hits| hits.get());
        let tree = load_session_tree(&path).expect("lenient load");
        // An unrecognized record shape must not break loading; it defers the
        // whole file to the lenient Value path and preserves the unknown
        // entry verbatim (future-proof tolerance).
        assert_eq!(
            LENIENT_LOAD_PATH_HITS.with(|hits| hits.get()),
            lenient_before + 1
        );
        assert_eq!(TYPED_LOAD_PATH_HITS.with(|hits| hits.get()), typed_before);
        assert!(
            tree.entries
                .iter()
                .any(|entry| entry.entry_type == "message")
        );
        assert!(
            tree.entries
                .iter()
                .any(|entry| entry.entry_type == "telemetry_snapshot")
        );
        assert_eq!(tree.entries.len(), 2);
    }

    /// A repeated non-empty entry id in a writer-shaped file must fail the
    /// single-pass typed load itself — never the lenient fallback — because
    /// aliased entries would self-parent during tree materialization. Resume
    /// must refuse the same file, and the rejection must never leak the
    /// session file's local path.
    #[test]
    fn typed_load_rejects_duplicate_entry_ids() {
        let directory = tempfile::tempdir().expect("directory");
        let path = directory.path().join("duplicate-typed.jsonl");
        let body = format!(
            "{}\n{}\n{}\n",
            session_header_line(directory.path()),
            serde_json::to_string(&json!({
                "type": "message",
                "id": "dup",
                "parentId": null,
                "timestamp": "2026-01-01T00:00:01.000Z",
                "message": Message::user_text("first", 0),
            }))
            .expect("first message row"),
            serde_json::to_string(&json!({
                "type": "message",
                "id": "dup",
                "parentId": null,
                "timestamp": "2026-01-01T00:00:02.000Z",
                "message": Message::user_text("second", 1),
            }))
            .expect("duplicate message row")
        );
        fs::write(&path, &body).expect("write duplicate session");

        let typed_before = TYPED_LOAD_PATH_HITS.with(|hits| hits.get());
        let lenient_before = LENIENT_LOAD_PATH_HITS.with(|hits| hits.get());
        let error = load_session_tree(&path).expect_err("duplicate ids must fail the typed load");
        let message = format!("{error:#}");
        assert!(
            message.contains("duplicate session entry id \"dup\""),
            "{message}"
        );
        // The rejection belongs to the typed fast path, not the fallback.
        assert_eq!(
            TYPED_LOAD_PATH_HITS.with(|hits| hits.get()),
            typed_before + 1
        );
        assert_eq!(
            LENIENT_LOAD_PATH_HITS.with(|hits| hits.get()),
            lenient_before
        );
        // Contextual and actionable, but never leaking the local path.
        assert!(
            !message.contains(path.to_str().expect("utf-8 path")),
            "{message}"
        );
        assert_eq!(
            fs::read_to_string(&path).expect("read duplicate session"),
            body
        );
        let resume_error = resume_session(&path).expect_err("duplicate ids must block resume");
        assert!(format!("{resume_error:#}").contains("duplicate session entry id \"dup\""));
    }

    /// An unknown record shape defers the whole file to the lenient Value
    /// path; that fallback must reject duplicate non-empty entry ids exactly
    /// like the typed loader, without mutating the file.
    #[test]
    fn lenient_load_rejects_duplicate_entry_ids() {
        let directory = tempfile::tempdir().expect("directory");
        let path = directory.path().join("duplicate-lenient.jsonl");
        let body = format!(
            "{}\n{}\n{}\n{}\n",
            session_header_line(directory.path()),
            serde_json::to_string(&json!({
                "type": "message",
                "id": "dup",
                "parentId": null,
                "timestamp": "2026-01-01T00:00:01.000Z",
                "message": Message::user_text("first", 0),
            }))
            .expect("first message row"),
            serde_json::to_string(&json!({
                "type": "telemetry_snapshot",
                "id": "dup",
                "parentId": null,
                "timestamp": "2026-01-01T00:00:02.000Z",
                "samples": [1, 2, 3],
            }))
            .expect("duplicate unknown row"),
            serde_json::to_string(&json!({
                "type": "message",
                "id": "other",
                "parentId": null,
                "timestamp": "2026-01-01T00:00:03.000Z",
                "message": Message::user_text("third", 2),
            }))
            .expect("unique message row")
        );
        fs::write(&path, &body).expect("write duplicate session");

        let typed_before = TYPED_LOAD_PATH_HITS.with(|hits| hits.get());
        let lenient_before = LENIENT_LOAD_PATH_HITS.with(|hits| hits.get());
        let error = load_session_tree(&path).expect_err("duplicate ids must fail the lenient load");
        let message = format!("{error:#}");
        assert!(
            message.contains("duplicate session entry id \"dup\""),
            "{message}"
        );
        // The unknown shape forces the lenient fallback, and the fallback
        // rejects the duplicate itself.
        assert_eq!(
            LENIENT_LOAD_PATH_HITS.with(|hits| hits.get()),
            lenient_before + 1
        );
        assert_eq!(TYPED_LOAD_PATH_HITS.with(|hits| hits.get()), typed_before);
        assert!(
            !message.contains(path.to_str().expect("utf-8 path")),
            "{message}"
        );
        assert_eq!(
            fs::read_to_string(&path).expect("read duplicate session"),
            body
        );
        let resume_error = resume_session(&path).expect_err("duplicate ids must block resume");
        assert!(format!("{resume_error:#}").contains("duplicate session entry id \"dup\""));
    }

    /// The typed fast path must skip `id: ""` records exactly like the
    /// lenient path. Two empty-id records plus a bushy `parentId: ""`
    /// subtree must therefore leave no empty `by_id` slot and no
    /// `children_by_parent[Some("")]` bucket; otherwise every empty-id node
    /// would materialize the same children through the aliased id.
    #[test]
    fn typed_load_skips_empty_id_records_and_never_indexes_aliased_children() {
        let directory = tempfile::tempdir().expect("directory");
        let path = directory.path().join("empty-ids-typed.jsonl");
        let mut lines = vec![session_header_line(directory.path())];
        for sequence in 0..2 {
            lines.push(
                serde_json::to_string(&json!({
                    "type": "message",
                    "id": "",
                    "parentId": null,
                    "timestamp": "2026-01-01T00:00:01.000Z",
                    "message": Message::user_text("empty", sequence),
                }))
                .expect("empty-id row"),
            );
        }
        for index in 0..4 {
            lines.push(
                serde_json::to_string(&json!({
                    "type": "message",
                    "id": format!("b{index}"),
                    "parentId": "",
                    "timestamp": "2026-01-01T00:00:02.000Z",
                    "message": Message::user_text("bushy", index),
                }))
                .expect("bushy child row"),
            );
        }
        fs::write(&path, lines.join("\n")).expect("write empty-id session");

        let typed_before = TYPED_LOAD_PATH_HITS.with(|hits| hits.get());
        let lenient_before = LENIENT_LOAD_PATH_HITS.with(|hits| hits.get());
        let tree = load_session_tree(&path).expect("empty ids must not fail the typed load");
        assert_eq!(
            TYPED_LOAD_PATH_HITS.with(|hits| hits.get()),
            typed_before + 1
        );
        assert_eq!(
            LENIENT_LOAD_PATH_HITS.with(|hits| hits.get()),
            lenient_before
        );

        assert_eq!(
            tree.entries.len(),
            4,
            "empty-id records are skipped entirely"
        );
        assert_eq!(tree.skipped_idless, 2);
        assert_eq!(tree.undecodable_messages, 0);
        assert_eq!(tree.by_id.len(), 4, "only non-empty ids are indexed");
        assert!(
            !tree.by_id.contains_key(""),
            "empty id must never be indexed"
        );
        assert!(
            !tree.children_by_parent.contains_key(&Some(String::new())),
            "parentId:\"\" must not resolve through an aliased empty id"
        );
        let forest = tree.tree();
        assert_eq!(
            forest.tree.len(),
            4,
            "every unresolved empty parent becomes one root"
        );
        assert!(forest.tree.iter().all(|node| node.children.is_empty()));
    }

    #[test]
    fn lenient_load_counts_undecodable_message_payloads() {
        let directory = tempfile::tempdir().expect("directory");
        let path = directory.path().join("undecodable-message.jsonl");
        let lines = [
            session_header_line(directory.path()),
            serde_json::to_string(&json!({
                "type": "message",
                "id": "bad-payload",
                "parentId": null,
                "timestamp": "2026-01-01T00:00:01.000Z",
                "message": "not-a-message-object",
            }))
            .expect("undecodable row"),
            serde_json::to_string(&json!({
                "type": "unknown_shape",
                "id": "force-lenient",
                "parentId": "bad-payload",
                "timestamp": "2026-01-01T00:00:02.000Z",
            }))
            .expect("unknown shape"),
        ];
        fs::write(&path, lines.join("\n")).expect("write undecodable session");
        let tree = load_session_tree(&path).expect("lenient load with undecodable payload");
        assert!(tree.undecodable_messages >= 1, "{tree:?}");
        assert_eq!(tree.skipped_idless, 0);
        assert!(
            !tree.entries.iter().any(|entry| entry.message.is_some()),
            "{:?}",
            tree.entries
        );
    }

    /// The duplicate guard must never fire for a legitimate deep unique
    /// chain: 20,001 uniquely identified entries still load, index, and
    /// materialize exactly as the direct-construction contract above.
    #[test]
    fn unique_deep_chain_loads_and_materializes() {
        const ENTRY_COUNT: usize = 20_001;
        let directory = tempfile::tempdir().expect("directory");
        let path = directory.path().join("unique-chain.jsonl");
        let mut lines = vec![session_header_line(directory.path())];
        for index in 0..ENTRY_COUNT {
            let parent_id = if index == 0 {
                Value::Null
            } else {
                json!(format!("e{}", index - 1))
            };
            lines.push(
                serde_json::to_string(&json!({
                    "type": "message",
                    "id": format!("e{index}"),
                    "parentId": parent_id,
                    "timestamp": "2026-01-01T00:00:00.000Z",
                    "message": Message::user_text("chain", index as i64),
                }))
                .expect("serialize chain row"),
            );
        }
        fs::write(&path, lines.join("\n")).expect("write unique chain");

        let tree = load_session_tree(&path).expect("unique deep chain must load");
        assert_eq!(tree.entries.len(), ENTRY_COUNT);
        assert_eq!(tree.by_id.len(), ENTRY_COUNT, "every unique id is indexed");
        assert_eq!(tree.tree().tree.len(), 1, "one root node");
        assert_eq!(tree.build_context(None).messages.len(), ENTRY_COUNT);
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
            todo_execution_status: None,
            todo_failure_reason: None,
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
            skipped_idless: 0,
            undecodable_messages: 0,
            by_id,
            children_by_parent,
            labels,
        }
    }

    #[test]
    fn empty_assistant_metadata_does_not_erase_recorded_model_change() {
        let mut model_entry = test_entry("model", None);
        model_entry.entry_type = "model_change".to_owned();
        model_entry.message = None;
        model_entry.provider = Some("recorded-provider".to_owned());
        model_entry.model_id = Some("recorded-model".to_owned());

        let mut assistant = pi_ai::AssistantMessage::pending(&Model::default());
        assistant.content = vec![ContentBlock::text("imported answer")];
        assistant.provider.clear();
        assistant.model.clear();
        let mut assistant_entry = test_entry("assistant", Some("model"));
        assistant_entry.message = Some(Message::Assistant(assistant));

        let context = test_tree(vec![model_entry, assistant_entry], Some("assistant")).build_context(None);
        assert_eq!(context.provider.as_deref(), Some("recorded-provider"));
        assert_eq!(context.model_id.as_deref(), Some("recorded-model"));
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
    fn generated_native_selected_leaf_keeps_exact_marker_out_of_padding() {
        let directory = tempfile::tempdir().expect("directory");
        let path = directory.path().join("p0-native-20mib.jsonl");
        let marker = "p0-native-20mib-marker";
        let lines = [
            json!({
                "type": "session",
                "version": CURRENT_SESSION_VERSION,
                "id": "p0-native-20mib",
                "timestamp": "2026-08-31T00:00:00.000Z",
                "cwd": directory.path(),
            }),
            json!({
                "type": "session_info",
                "id": "si-1",
                "parentId": null,
                "timestamp": "2026-08-31T00:00:00.000Z",
                "name": "p0-native-20mib",
            }),
            json!({
                "type": "message",
                "id": "m-off",
                "parentId": "si-1",
                "timestamp": "2026-08-31T00:00:00.000Z",
                "message": Message::user_text("x".repeat(64), 0),
            }),
            json!({
                "type": "message",
                "id": "m-marker",
                "parentId": "si-1",
                "timestamp": "2026-08-31T00:00:00.000Z",
                "message": Message::user_text(marker, 1),
            }),
        ];
        fs::write(
            &path,
            format!(
                "{}\n",
                lines
                    .iter()
                    .map(|line| serde_json::to_string(line).expect("serialize generated native row"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
        )
        .expect("write generated native session");

        let tree = load_session_tree(&path).expect("load generated native tree");
        assert_eq!(tree.active_leaf_id.as_deref(), Some("m-marker"));
        assert_eq!(tree.leaf_id.as_deref(), Some("m-marker"));
        let context = tree.build_context(None);
        assert_eq!(message_texts(&context), vec![marker.to_owned()]);
        let encoded = serde_json::to_string(&context.messages).expect("encode public messages");
        assert!(
            encoded.contains(marker),
            "selected-branch public messages must carry the exact marker: {encoded}"
        );
        assert!(
            !encoded.contains("xxxxxxxx"),
            "off-branch padding must not reconstruct into build_context(None): {encoded}"
        );
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
        assert_eq!(
            message_texts(&reloaded.build_context(None)),
            vec!["a", "b", "d", "e"]
        );

        fs::remove_dir_all(directory).expect("remove test directory");
    }
    #[test]
    fn malformed_jsonl_record_fails_load_and_resume() {
        for corrupt_record in ["{not-json}", "[]"] {
            let directory =
                std::env::temp_dir().join(format!("pi-session-corrupt-{}", Uuid::new_v4()));
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
    fn malformed_todo_snapshot_fails_load_and_resume() {
        let directory =
            std::env::temp_dir().join(format!("pi-session-corrupt-todo-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).expect("create test directory");
        let path = directory.join("session.jsonl");
        let lines = [
            json!({
                "type": "session",
                "version": CURRENT_SESSION_VERSION,
                "id": "corrupt-todo-session",
                "timestamp": "2026-01-01T00:00:00.000Z",
                "cwd": directory,
            }),
            json!({
                "type": "todo_snapshot",
                "id": "bad-todo",
                "parentId": null,
                "timestamp": "2026-01-01T00:00:01.000Z",
                "state": { "phases": "not-an-array", "storage": "session" },
            }),
        ];
        fs::write(
            &path,
            lines
                .iter()
                .map(|line| serde_json::to_string(line).expect("serialize record"))
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .expect("write malformed todo snapshot");

        let load_error = load_session_tree(&path).expect_err("reject malformed todo snapshot");
        assert!(
            load_error
                .to_string()
                .contains("decoding todo_snapshot bad-todo")
        );
        let resume_error = resume_session(&path).expect_err("reject malformed todo resume");
        assert!(
            resume_error
                .to_string()
                .contains("decoding todo_snapshot bad-todo")
        );

        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn resume_defers_final_record_separator_until_append() {
        for suffix in ["", "\r"] {
            let directory =
                std::env::temp_dir().join(format!("pi-session-newline-{}", Uuid::new_v4()));
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
            let preserved_modified = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
            File::options()
                .write(true)
                .open(&path)
                .expect("open session timestamp")
                .set_times(std::fs::FileTimes::new().set_modified(preserved_modified))
                .expect("set session timestamp");
            let original = fs::read(&path).expect("read original session");

            let recorder = resume_session(&path).expect("resume session without append");
            assert_eq!(fs::read(&path).expect("read resumed session"), original);
            assert_eq!(
                fs::metadata(&path)
                    .expect("resumed metadata")
                    .modified()
                    .expect("resumed mtime"),
                preserved_modified
            );
            recorder.close().expect("close resumed recorder");
            assert_eq!(fs::read(&path).expect("read closed session"), original);
            assert_eq!(
                fs::metadata(&path)
                    .expect("closed metadata")
                    .modified()
                    .expect("closed mtime"),
                preserved_modified
            );

            let recorder = resume_session(&path).expect("resume session for append");
            recorder.inner.lock().file = Some(File::open(&path).expect("open read-only session"));
            recorder
                .record_message(&Message::user_text("second", 0))
                .expect_err("read-only append must fail");
            assert_eq!(
                fs::read(&path).expect("read failed append session"),
                original
            );
            {
                let mut state = recorder.inner.lock();
                assert!(state.requires_separator);
                assert!(state.pending.is_empty());
                state.file = Some(
                    File::options()
                        .read(true)
                        .append(true)
                        .open(&path)
                        .expect("reopen writable session"),
                );
            }
            recorder
                .record_message(&Message::user_text("second", 0))
                .expect("retry append message");
            recorder.close().expect("close appended recorder");

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
        let branch_record: SessionRecord =
            serde_json::from_value(branch.clone()).expect("deserialize branch record");
        assert_eq!(
            serde_json::to_value(branch_record).expect("serialize branch record"),
            branch
        );

        let compaction = json!({
            "type":"compaction", "id":"compaction-id", "parentId":"branch-id",
            "timestamp":"2026-01-01T00:00:02.000Z", "summary":"compact", "tokensBefore":9,
            "details":{"modifiedFiles":["src/main.rs"]},
            "usage":{"input":3,"output":4,"cacheRead":0,"cacheWrite":0,"cacheWrite1h":0,"reasoning":0,"totalTokens":7,"cost":{"input":0.0,"output":0.0,"cacheRead":0.0,"cacheWrite":0.0,"total":0.0}},
            "fromHook":false
        });
        let compaction_record: SessionRecord =
            serde_json::from_value(compaction.clone()).expect("deserialize compaction record");
        assert_eq!(
            serde_json::to_value(compaction_record).expect("serialize compaction record"),
            compaction
        );
        assert_eq!(encoded["type"], "label");
        assert_eq!(encoded["parentId"], "entry-id");
        assert!(encoded.get("label").is_none());
        let decoded: SessionRecord =
            serde_json::from_value(encoded).expect("deserialize session record");
        assert_eq!(decoded, record);
    }

    #[test]
    fn summary_metadata_round_trips_entries_context_and_recorder() {
        let directory = tempfile::tempdir().expect("directory");
        let recorder = start_session_in(
            directory.path(),
            None,
            None,
            Some(directory.path()),
            Some("summary-metadata"),
            None,
        )
        .expect("start session");
        let root_id = recorder
            .record_message(&Message::user_text("root", 0))
            .expect("record root");
        let branch_details = json!({"readFiles":["src/lib.rs"]});
        let branch_usage = Usage {
            input: 11,
            output: 3,
            total_tokens: 14,
            ..Usage::default()
        };
        let branch_id = recorder
            .branch_with_summary_metadata(
                Some(&root_id),
                "branch summary",
                Some(&branch_details),
                Some(&branch_usage),
                Some(true),
            )
            .expect("record branch summary");
        let compaction_details = json!({"modifiedFiles":["src/main.rs"]});
        let compaction_usage = Usage {
            input: 21,
            output: 5,
            total_tokens: 26,
            ..Usage::default()
        };
        let compaction_id = recorder
            .record_compaction_metadata(
                "compaction summary",
                None,
                1234,
                &[],
                Some(&compaction_details),
                Some(&compaction_usage),
                Some(false),
            )
            .expect("record compaction");
        recorder.close().expect("close recorder");

        let tree = load_session_tree(recorder.path()).expect("reload summaries");
        let branch_entry = tree
            .entries
            .iter()
            .find(|entry| entry.id == branch_id)
            .expect("branch entry");
        assert_eq!(branch_entry.details.as_ref(), Some(&branch_details));
        assert_eq!(branch_entry.usage.as_ref(), Some(&branch_usage));
        assert_eq!(branch_entry.from_hook, Some(true));
        assert!(
            matches!(tree.build_context(Some(&branch_id)).messages.last(),
            Some(Message::BranchSummary(message))
                if message.details.as_ref() == Some(&branch_details)
                    && message.usage.as_ref() == Some(&branch_usage)
                    && message.from_hook == Some(true))
        );

        let compaction_entry = tree
            .entries
            .iter()
            .find(|entry| entry.id == compaction_id)
            .expect("compaction entry");
        assert_eq!(compaction_entry.details.as_ref(), Some(&compaction_details));
        assert_eq!(compaction_entry.usage.as_ref(), Some(&compaction_usage));
        assert_eq!(compaction_entry.from_hook, Some(false));
        assert!(
            matches!(tree.build_context(Some(&compaction_id)).messages.first(),
            Some(Message::CompactionSummary(message))
                if message.details.as_ref() == Some(&compaction_details)
                    && message.usage.as_ref() == Some(&compaction_usage)
                    && message.from_hook == Some(false))
        );

        let legacy = start_session_in(
            directory.path(),
            None,
            None,
            Some(directory.path()),
            Some("summary-legacy"),
            None,
        )
        .expect("legacy recorder");
        legacy
            .record_compaction("legacy", None, 7, &[])
            .expect("legacy compaction");
        legacy.close().expect("close legacy recorder");
        let legacy_rows = fs::read_to_string(legacy.path()).expect("read legacy rows");
        let legacy_compaction = legacy_rows
            .lines()
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
            lines
                .iter()
                .map(|line| serde_json::to_string(line).expect("serialize source row"))
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .expect("write source");

        let fork = create_branched_session(&source_path, "c").expect("create branch");
        let fork_path = fork.path();
        fork.close().expect("close branch");
        let tree = load_session_tree(&fork_path).expect("load branch");
        assert_eq!(tree.tree().tree.len(), 1);
        assert_eq!(tree.active_leaf_id.as_deref(), Some("c"));
        assert_eq!(
            message_texts(&tree.build_context(None)),
            vec!["a", "b", "c"]
        );

        let normal = tree
            .entries
            .iter()
            .filter(|entry| entry.entry_type != "label")
            .collect::<Vec<_>>();
        assert_eq!(
            normal
                .iter()
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
        assert_eq!(normal[0].parent_id, None);
        assert_eq!(normal[1].parent_id.as_deref(), Some("a"));
        assert_eq!(normal[2].parent_id.as_deref(), Some("b"));

        let labels = tree
            .entries
            .iter()
            .filter(|entry| entry.entry_type == "label")
            .collect::<Vec<_>>();
        assert_eq!(labels.len(), 2);
        assert!(labels.iter().all(|entry| !matches!(
            entry.id.as_str(),
            "label-a" | "clear-a" | "old-b" | "new-b" | "label-c"
        )));
        let all_ids = tree
            .entries
            .iter()
            .map(|entry| entry.id.as_str())
            .collect::<HashSet<_>>();
        assert_eq!(all_ids.len(), tree.entries.len());
        for pair in tree.entries.windows(2) {
            assert_eq!(pair[1].parent_id.as_deref(), Some(pair[0].id.as_str()));
        }
        assert_eq!(tree.labels.get("a").map(|label| label.label.as_str()), None);
        assert_eq!(
            tree.labels.get("b").map(|label| label.label.as_str()),
            Some("current")
        );
        assert_eq!(
            tree.labels.get("b").map(|label| label.timestamp.as_str()),
            Some("2026-01-01T00:00:06.000Z")
        );
        assert_eq!(
            tree.labels.get("c").map(|label| label.label.as_str()),
            Some("leaf")
        );
        assert_eq!(
            tree.labels.get("c").map(|label| label.timestamp.as_str()),
            Some("2026-01-01T00:00:08.000Z")
        );
    }

    #[test]
    fn custom_records_omit_absent_optional_metadata() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let recorder = start_session_in(
            workspace.path(),
            None,
            None,
            Some(sessions.path()),
            None,
            None,
        )
        .expect("start session");
        recorder
            .record_custom_entry("extension.state", None)
            .expect("record custom state");
        recorder
            .record_custom_message(&CustomMessage {
                custom_type: "extension.notice".into(),
                content: "visible".into(),
                display: true,
                details: None,
                timestamp: 123,
            })
            .expect("record custom message");
        recorder
            .record_message(&Message::Assistant(pi_ai::AssistantMessage::pending(
                &Model::default(),
            )))
            .expect("flush session");
        recorder.close().expect("close recorder");

        let records = fs::read_to_string(recorder.path()).expect("read session");
        let rows = records
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).expect("parse row"))
            .collect::<Vec<_>>();
        let state = rows
            .iter()
            .find(|row| row["type"] == "custom")
            .expect("custom row");
        assert!(state.get("data").is_none());
        let message = rows
            .iter()
            .find(|row| row["type"] == "custom_message")
            .expect("custom message row");
        assert!(message.get("details").is_none());
        assert!(message.get("role").is_none());
        assert!(message["timestamp"].as_str().is_some());
    }

    #[test]
    fn custom_entries_round_trip_and_project_with_original_metadata() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let recorder = start_session_in(
            workspace.path(),
            None,
            None,
            Some(sessions.path()),
            None,
            None,
        )
        .expect("start session");
        recorder
            .record_custom_entry("extension.state", Some(json!({"cursor":7})))
            .expect("record custom state");
        let custom = CustomMessage {
            custom_type: "extension.notice".into(),
            content: CustomMessageContent::Blocks(vec![
                ContentBlock::text("remember"),
                ContentBlock::Image {
                    data: "aGVsbG8=".into(),
                    mime_type: "image/png".into(),
                },
            ]),
            display: false,
            details: Some(json!({"nested":{"preserved":true}})),
            timestamp: 123,
        };
        recorder
            .record_custom_message(&custom)
            .expect("record custom message");
        recorder
            .record_message(&Message::Assistant(pi_ai::AssistantMessage::pending(
                &Model::default(),
            )))
            .expect("flush session");
        recorder.close().expect("close recorder");

        let tree = load_session_tree(recorder.path()).expect("reload custom session");
        let state = tree
            .entries
            .iter()
            .find(|entry| entry.entry_type == "custom")
            .expect("custom state entry");
        assert_eq!(state.custom_type.as_deref(), Some("extension.state"));
        assert_eq!(state.data, Some(json!({"cursor":7})));
        let entry = tree
            .entries
            .iter()
            .find(|entry| entry.entry_type == "custom_message")
            .expect("custom message entry");
        assert_eq!(entry.custom_type.as_deref(), Some("extension.notice"));
        assert_eq!(entry.display, Some(false));
        assert_eq!(entry.details, custom.details);
        assert_eq!(entry.content, Some(custom.content.clone()));
        assert!(
            matches!(&tree.build_context(None).messages[0], Message::Custom(message)
            if message.custom_type == custom.custom_type
                && message.content == custom.content
                && !message.display
                && message.details == custom.details)
        );
    }

    #[test]
    fn handwritten_orchestration_custom_survives_typed_tree_reload() {
        let directory = tempfile::tempdir().expect("directory");
        let path = directory.path().join("web-transcript-parity.jsonl");
        let stamp = "2026-01-01T00:00:00.000Z";
        fs::write(
            &path,
            [
                json!({
                    "type": "session",
                    "version": CURRENT_SESSION_VERSION,
                    "id": "web-transcript-parity",
                    "timestamp": stamp,
                    "cwd": directory.path(),
                }),
                json!({
                    "type": "custom_message",
                    "id": "irc",
                    "parentId": serde_json::Value::Null,
                    "timestamp": stamp,
                    "customType": "orchestration_message",
                    "content": "<orchestration-message id=\"m1\" from=\"Main\">\nclean IRC parity body\n</orchestration-message>",
                    "display": true,
                    "details": {
                        "id": "m1",
                        "from": "Main",
                        "to": "Child",
                        "body": "clean IRC parity body",
                    },
                }),
            ]
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
                + "\n",
        )
        .expect("write handwritten custom jsonl");

        let typed_before = TYPED_LOAD_PATH_HITS.with(|hits| hits.get());
        let lenient_before = LENIENT_LOAD_PATH_HITS.with(|hits| hits.get());
        let disk = load_session_tree(&path).expect("load handwritten custom");
        assert!(
            TYPED_LOAD_PATH_HITS.with(|hits| hits.get()) == typed_before + 1
                || LENIENT_LOAD_PATH_HITS.with(|hits| hits.get()) == lenient_before + 1,
            "handwritten custom jsonl must take typed or lenient load"
        );
        let irc = disk
            .entries
            .iter()
            .find(|entry| entry.id == "irc")
            .expect("irc entry");
        assert_eq!(irc.entry_type, "custom_message");
        assert_eq!(irc.custom_type.as_deref(), Some("orchestration_message"));
        assert_eq!(irc.display, Some(true));
        assert!(
            matches!(&irc.content, Some(CustomMessageContent::Text(text) ) if text.contains("clean IRC parity body")),
            "{irc:?}"
        );

        let prepared = PreparedSessionResume::prepare_path(&path).expect("prepare");
        let recorder = prepared.into_recorder().expect("recorder");
        recorder.activate_writer().expect("activate writer");
        let reloaded = recorder.tree().expect("recorder tree reload");
        let reloaded_irc = reloaded
            .entries
            .iter()
            .find(|entry| entry.id == "irc")
            .expect("reloaded irc");
        assert_eq!(reloaded_irc.custom_type.as_deref(), Some("orchestration_message"));
        assert_eq!(reloaded_irc.display, Some(true));
        assert!(
            matches!(&reloaded_irc.content, Some(CustomMessageContent::Text(text) ) if text.contains("clean IRC parity body")),
            "{reloaded_irc:?}"
        );
        let mut projected = Vec::new();
        append_entry_message(&mut projected, reloaded_irc);
        assert!(
            matches!(
                projected.first(),
                Some(Message::Custom(custom))
                    if custom.display && custom.custom_type == "orchestration_message"
            ),
            "{projected:?}"
        );
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
        fs::write(
            &path,
            format!("{}\n", serde_json::to_string(&header).expect("header")),
        )
        .expect("write session");

        let recorder = resume_session(&path).expect("resume recorder");
        assert_eq!(
            recorder
                .record_session_name("  Alpha\r\nBeta  ")
                .expect("record name")
                .as_deref(),
            Some("Alpha Beta")
        );
        assert_eq!(recorder.session_name().as_deref(), Some("Alpha Beta"));
        assert_eq!(
            load_session_tree(&path)
                .expect("load named")
                .session_name()
                .as_deref(),
            Some("Alpha Beta")
        );

        assert_eq!(
            recorder.record_session_name(" \n ").expect("clear name"),
            None
        );
        assert_eq!(recorder.session_name(), None);
        assert_eq!(
            load_session_tree(&path)
                .expect("load cleared")
                .session_name(),
            None
        );
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn bash_execution_persists_and_loads_without_role_conversion() {
        let directory = std::env::temp_dir().join(format!("pi-session-bash-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).expect("create test directory");
        let path = directory.join("session.jsonl");
        let bash = Message::BashExecution(pi_ai::BashExecutionMessage {
            command: "echo saved".into(),
            output: "saved".into(),
            exit_code: Some(0),
            cancelled: false,
            truncated: false,
            full_output_path: None,
            timestamp: 42,
            exclude_from_context: Some(true),
        });
        let lines = [
            json!({"type":"session", "version":CURRENT_SESSION_VERSION, "id":"test-session", "timestamp":"2026-01-01T00:00:00.000Z", "cwd":directory}),
            json!({"type":"message", "id":"a", "parentId":null, "timestamp":"2026-01-01T00:00:01.000Z", "message":bash}),
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

        let tree = load_session_tree(&path).expect("load session");
        assert!(
            matches!(tree.entries[0].message, Some(Message::BashExecution(ref message))
            if message.command == "echo saved" && message.exclude_from_context == Some(true))
        );
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
        assert_eq!(
            list_sessions_in(cwd.path(), Some(sessions.path()))[0].id,
            "exact_id"
        );

        let forked = fork_session_in(&source, cwd.path(), Some(sessions.path()), Some("fork_id"))
            .expect("fork explicit session");
        assert_eq!(forked.id(), "fork_id");
        forked.close().expect("close fork");

        let source_recorder = resume_session(&source).expect("resume source for labels");
        let leaf = source_recorder
            .record_message(&Message::user_text("labeled leaf", 2))
            .expect("record labeled leaf");
        source_recorder
            .record_label(&leaf, Some("checkpoint"))
            .expect("label leaf");
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
        assert_eq!(
            fork_tree
                .labels
                .get(&leaf)
                .map(|label| label.label.as_str()),
            Some("checkpoint")
        );
        assert_eq!(fork_tree.tree().tree.len(), 1);
        for pair in fork_tree.entries.windows(2) {
            assert_eq!(pair[1].parent_id.as_deref(), Some(pair[0].id.as_str()));
        }
        assert_eq!(list_sessions_in(cwd.path(), Some(sessions.path())).len(), 3);
    }

    #[test]
    fn session_ids_reject_path_syntax_and_duplicates() {
        for invalid in [
            "",
            ".hidden",
            "../escape",
            "bad/name",
            "bad\\name",
            "space id",
        ] {
            assert!(
                validate_session_id(invalid).is_err(),
                "accepted {invalid:?}"
            );
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
        assert!(
            start_session_in(
                cwd.path(),
                None,
                None,
                Some(sessions.path()),
                Some("duplicate"),
                None,
            )
            .is_err()
        );
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
        let second_mtime = fs::metadata(&newer_path)
            .expect("second metadata")
            .modified()
            .expect("second mtime");
        for _ in 0..100 {
            std::thread::sleep(Duration::from_millis(2));
            fs::write(&older_path, &older_body).expect("touch first session");
            if fs::metadata(&older_path)
                .expect("first metadata")
                .modified()
                .expect("first mtime")
                > second_mtime
            {
                break;
            }
        }
        assert!(
            fs::metadata(&older_path)
                .expect("first metadata")
                .modified()
                .expect("first mtime")
                > second_mtime,
            "test filesystem did not advance mtime"
        );
        let listed = list_sessions_in(cwd.path(), Some(directory.path()));
        assert_eq!(
            listed
                .iter()
                .map(|session| session.id.as_str())
                .collect::<Vec<_>>(),
            ["mtime-wins", "timestamp-loses"]
        );

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
        recorder.branch("a").expect("move to root entry");
        let branch_id = recorder
            .record_message(&Message::user_text("second branch", 0))
            .expect("append branch");
        recorder.reset_leaf();
        let root_id = recorder
            .record_message(&Message::user_text("second root", 0))
            .expect("append root");
        recorder.close().expect("close recorder");
        let tree = load_session_tree(&path).expect("reload");
        assert_eq!(
            tree.entries
                .iter()
                .find(|entry| entry.id == branch_id)
                .and_then(|entry| entry.parent_id.as_deref()),
            Some("a")
        );
        assert_eq!(
            tree.entries
                .iter()
                .find(|entry| entry.id == root_id)
                .and_then(|entry| entry.parent_id.as_deref()),
            None
        );
        assert_eq!(tree.tree().tree.len(), 2);
        assert_eq!(
            fs::read_to_string(&path)
                .expect("read file")
                .lines()
                .count(),
            5
        );
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
        let active_leaf = recorder.active_leaf_id();
        recorder
            .record_label("a", Some("checkpoint"))
            .expect("set label");
        let labeled = recorder.tree().expect("labeled tree").tree();
        assert_eq!(labeled.tree[0].label.as_deref(), Some("checkpoint"));
        assert!(labeled.tree[0].label_timestamp.is_some());
        assert_eq!(recorder.active_leaf_id(), active_leaf);
        recorder.record_label("a", None).expect("clear label");
        assert_eq!(recorder.tree().expect("cleared tree").tree().tree[0].label, None);
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
        assert_eq!(
            message_texts(&clone.tree().expect("clone tree").build_context(None)),
            vec!["root"]
        );
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
                tasks: vec![
                    crate::TodoItem {
                        id: "task-compile".to_owned(),
                        content: "compile".to_owned(),
                        status: crate::TodoStatus::InProgress,
                        depends_on: Vec::new(),
                        ready: true,
                        waiting_on: Vec::new(),
                        agent: None,
                    },
                    crate::TodoItem {
                        id: "task-test".to_owned(),
                        content: "test".to_owned(),
                        status: crate::TodoStatus::Pending,
                        depends_on: vec!["task-compile".to_owned()],
                        ready: false,
                        waiting_on: vec![crate::TodoWaitingOn {
                            task_id: "task-compile".to_owned(),
                            content: "compile".to_owned(),
                            status: crate::TodoStatus::InProgress,
                        }],
                        agent: None,
                    },
                ],
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
        assert_eq!(
            recorder.latest_todo_state().expect("latest"),
            Some(state.clone())
        );
        let replacement = crate::TodoState {
            phases: Vec::new(),
            storage: crate::TodoStorage::Session,
        };
        recorder
            .record_todo_snapshot(&replacement)
            .expect("record snapshot");
        assert_eq!(
            recorder.latest_todo_state().expect("updated"),
            Some(replacement)
        );
        recorder.close().expect("close recorder");
        assert_eq!(
            load_session_tree(&path)
                .expect("reload")
                .latest_todo_state(),
            Some(crate::TodoState {
                phases: Vec::new(),
                storage: crate::TodoStorage::Session
            })
        );
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn todo_snapshot_execution_status_and_failure_reason_round_trip_on_wire() {
        let directory =
            std::env::temp_dir().join(format!("pi-session-todo-execution-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).expect("create test directory");
        let path = directory.join("session.jsonl");
        let reason = crate::TodoDagFailureReason::NoProgress {
            message: "open tasks remain but none are ready".to_owned(),
        };
        let lines = [
            json!({"type":"session", "version":CURRENT_SESSION_VERSION, "id":"todo-execution", "timestamp":"2026-01-01T00:00:00.000Z", "cwd":directory}),
            json!({"type":"todo_snapshot", "id":"todo-a", "parentId":null, "timestamp":"2026-01-01T00:00:01.000Z", "state":{"phases":[],"storage":"session"}, "executionStatus":"failed", "failureReason":reason}),
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

        let snapshot = load_session_tree(&path)
            .expect("load Todo execution snapshot")
            .latest_todo_snapshot()
            .expect("Todo snapshot");
        assert_eq!(
            snapshot.execution_status,
            Some(crate::TodoDagExecutionStatus::Failed)
        );
        assert_eq!(snapshot.failure_reason, Some(reason));
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn todo_snapshot_unknown_execution_status_is_rejected() {
        let directory =
            std::env::temp_dir().join(format!("pi-session-todo-invalid-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).expect("create test directory");
        let path = directory.join("session.jsonl");
        let lines = [
            json!({"type":"session", "version":CURRENT_SESSION_VERSION, "id":"todo-invalid", "timestamp":"2026-01-01T00:00:00.000Z", "cwd":directory}),
            json!({"type":"todo_snapshot", "id":"todo-a", "parentId":null, "timestamp":"2026-01-01T00:00:01.000Z", "state":{"phases":[],"storage":"session"}, "executionStatus":"reviving"}),
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

        let error = load_session_tree(&path).expect_err("unknown execution status must fail closed");
        assert!(
            error.to_string().contains("executionStatus"),
            "unexpected error: {error:#}"
        );
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn legacy_todo_snapshot_migrates_stable_ids_on_reload() {
        let directory =
            std::env::temp_dir().join(format!("pi-session-todo-legacy-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).expect("create test directory");
        let path = directory.join("session.jsonl");
        let lines = [
            json!({"type":"session", "version":CURRENT_SESSION_VERSION, "id":"legacy-todo", "timestamp":"2026-01-01T00:00:00.000Z", "cwd":directory}),
            json!({"type":"message", "id":"a", "parentId":null, "timestamp":"2026-01-01T00:00:01.000Z", "message":Message::user_text("root", 0)}),
            json!({"type":"todo_snapshot", "id":"todo-a", "parentId":"a", "timestamp":"2026-01-01T00:00:02.000Z", "state":{"phases":[{"name":"Build","tasks":[{"content":"compile","status":"in_progress"},{"content":"test","status":"pending"}]}],"storage":"session"}}),
        ];
        fs::write(
            &path,
            lines
                .iter()
                .map(|line| serde_json::to_string(line).expect("serialize record"))
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .expect("write legacy session");
        let first = load_session_tree(&path)
            .expect("first load")
            .latest_todo_state()
            .expect("todo state");
        let second = load_session_tree(&path)
            .expect("second load")
            .latest_todo_state()
            .expect("todo state");
        assert_eq!(first.phases[0].tasks[0].id, second.phases[0].tasks[0].id);
        assert_eq!(first.phases[0].tasks[1].id, second.phases[0].tasks[1].id);
        assert!(
            first.phases[0]
                .tasks
                .iter()
                .all(|task| task.id.starts_with("task-") && task.ready)
        );
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn branch_summary_failed_write_restores_leaf_and_pending_log() {
        let directory = std::env::temp_dir().join(format!("pi-session-summary-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).expect("create test directory");
        let occupied = directory.join("session.jsonl");
        fs::write(&occupied, b"occupied").expect("occupy path");
        let recorder = SessionRecorder {
            inner: Arc::new(Mutex::new(RecorderState {
                path: occupied,
                id: "summary-session".to_owned(),
                file_identity: None,
                timestamp: "2026-01-01T00:00:00.000Z".to_owned(),
                cwd: directory.clone(),
                parent_session: None,
                last_id: Some("b".to_owned()),
                active_leaf_id: Some("b".to_owned()),
                revision: 0,
                used_ids: HashSet::from(["a".to_owned(), "b".to_owned()]),
                pending: vec![
                    json!({"type":"session", "version":CURRENT_SESSION_VERSION, "id":"summary-session", "timestamp":"2026-01-01T00:00:00.000Z", "cwd":directory}),
                ],
                requires_separator: false,
                file: None,
                lease: None,
                flushed: false,
                has_assistant: true,
                session_name: None,
                durable: false,
            })),
        };
        assert!(
            recorder
                .branch_with_summary(Some("a"), "cannot persist")
                .is_err()
        );
        assert_eq!(recorder.active_leaf_id().as_deref(), Some("b"));
        assert_eq!(recorder.inner.lock().pending.len(), 1);
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn tree_orders_multiple_roots_and_children_by_timestamp() {
        let mut late = test_entry("late", None);
        late.timestamp = "2026-01-01T00:00:03.000Z".to_owned();
        let mut child_late = test_entry("child-late", Some("late"));
        child_late.timestamp = "2026-01-01T00:00:05.000Z".to_owned();
        let mut early = test_entry("early", None);
        early.timestamp = "2026-01-01T00:00:01.000Z".to_owned();
        let mut child_early = test_entry("child-early", Some("late"));
        child_early.timestamp = "2026-01-01T00:00:04.000Z".to_owned();
        let tree = test_tree(
            vec![late, child_late, early, child_early],
            Some("child-late"),
        );
        let roots = tree.tree();
        assert_eq!(
            roots
                .tree
                .iter()
                .map(|node| node.entry.id.as_str())
                .collect::<Vec<_>>(),
            ["early", "late"]
        );
        assert_eq!(
            roots.tree[1]
                .children
                .iter()
                .map(|node| node.entry.id.as_str())
                .collect::<Vec<_>>(),
            ["child-early", "child-late"]
        );
    }

    fn set_modified_epoch(path: &Path, epoch: u64) {
        let file = File::options().write(true).open(path).expect("open");
        file.set_times(
            std::fs::FileTimes::new()
                .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(epoch)),
        )
        .expect("set mtime");
    }

    fn write_session_file(path: &Path, id: &str) {
        fs::write(
            path,
            native_session_body(path.parent().expect("parent"), id, id),
        )
        .expect("write session");
    }

    const PRUNE_NOW_EPOCH: u64 = 1_800_000_000;
    const PRUNE_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);
    const OLD_EPOCH: u64 = PRUNE_NOW_EPOCH - 40 * 24 * 60 * 60; // 40 days ago
    const FRESH_EPOCH: u64 = PRUNE_NOW_EPOCH - 24 * 60 * 60; // 1 day ago

    #[test]
    fn prune_removes_only_old_native_sessions_keeps_new_foreign_current_and_sidecars() {
        let tree = tempfile::tempdir().expect("native tree");
        let per_cwd = tree.path().join("--proj--");
        let parent_dir = tree.path().join("children").join("parent-1");
        let gone_dir = tree.path().join("--gone--");
        let sidecar_dir = tree.path().join("--sidecar-only--");
        fs::create_dir_all(&per_cwd).expect("per-cwd dir");
        fs::create_dir_all(&parent_dir).expect("child dir");
        fs::create_dir_all(&gone_dir).expect("gone dir");
        fs::create_dir_all(&sidecar_dir).expect("sidecar dir");

        let old = per_cwd.join("old.jsonl");
        let child_old = parent_dir.join("child-old.jsonl");
        let direct = tree.path().join("direct.jsonl");
        let only_old = gone_dir.join("only-old.jsonl");
        let sidecar_only = sidecar_dir.join("sidecar-only.jsonl");
        let fresh = per_cwd.join("fresh.jsonl");
        let current = per_cwd.join("current.jsonl");
        let child_new = parent_dir.join("child-new.jsonl");
        let notes = per_cwd.join("notes.txt");
        let sidecar = per_cwd.join("old.jsonl.loops.json");
        let kept_sidecar = sidecar_dir.join("sidecar-only.jsonl.loops.json");
        for path in [
            &old,
            &child_old,
            &direct,
            &only_old,
            &sidecar_only,
            &fresh,
            &current,
            &child_new,
        ] {
            write_session_file(path, "s");
        }
        fs::write(&notes, "not a session").expect("notes");
        fs::write(&sidecar, "{}").expect("sidecar");
        fs::write(&kept_sidecar, "{}").expect("kept sidecar");

        // Foreign tree: old sessions outside the pruned roots must survive.
        let foreign = tempfile::tempdir().expect("foreign tree");
        let foreign_old = foreign.path().join("rollout-old.jsonl");
        write_session_file(&foreign_old, "foreign");

        let now = std::time::UNIX_EPOCH + Duration::from_secs(PRUNE_NOW_EPOCH);
        for path in [
            &old,
            &child_old,
            &direct,
            &only_old,
            &sidecar_only,
            &current,
            &notes,
            &sidecar,
            &kept_sidecar,
            &foreign_old,
        ] {
            set_modified_epoch(path, OLD_EPOCH);
        }
        set_modified_epoch(&fresh, FRESH_EPOCH);
        set_modified_epoch(&child_new, FRESH_EPOCH);

        let deleted = prune_expired_sessions(
            &[tree.path().to_path_buf(), tree.path().to_path_buf()], // duplicate roots must dedupe
            now,
            PRUNE_TTL,
            &[current.clone()],
            &[],
        );
        assert_eq!(
            deleted, 5,
            "old per-cwd, old child, direct-in-root, only-old, and sidecar-only files go"
        );
        for path in [&old, &child_old, &direct, &only_old, &sidecar_only] {
            assert!(!path.exists(), "{} must be pruned", path.display());
        }
        assert!(!gone_dir.exists(), "emptied per-cwd dir must be removed");
        for path in [
            &fresh,
            &current,
            &child_new,
            &notes,
            &sidecar,
            &kept_sidecar,
        ] {
            assert!(path.exists(), "{} must survive", path.display());
        }
        assert!(
            per_cwd.exists(),
            "dir with surviving sessions/sidecar stays"
        );
        assert!(
            tree.path().join("children").exists(),
            "children root stays while parent-1 is non-empty"
        );
        assert!(parent_dir.exists(), "child dir with a fresh session stays");
        assert!(
            sidecar_dir.exists(),
            "dir holding a loop sidecar stays after its session file is pruned"
        );
        assert!(foreign_old.exists(), "foreign tree is never touched");
    }

    #[test]
    fn prune_skips_recent_files_within_grace_even_when_older_than_ttl() {
        let tree = tempfile::tempdir().expect("tree");
        let within_grace = tree.path().join("recent.jsonl");
        let truly_old = tree.path().join("old.jsonl");
        write_session_file(&within_grace, "recent");
        write_session_file(&truly_old, "old");
        let now = std::time::UNIX_EPOCH + Duration::from_secs(PRUNE_NOW_EPOCH);
        set_modified_epoch(&within_grace, PRUNE_NOW_EPOCH - 30 * 60); // 30 min ago
        set_modified_epoch(&truly_old, PRUNE_NOW_EPOCH - 2 * 60 * 60); // 2 h ago
        let ttl = Duration::from_secs(10 * 60); // smaller than the grace window
        let deleted = prune_expired_sessions(&[tree.path().to_path_buf()], now, ttl, &[], &[]);
        assert_eq!(deleted, 1);
        assert!(within_grace.exists(), "grace window must override TTL");
        assert!(
            !truly_old.exists(),
            "past both grace and TTL must be pruned"
        );
    }

    #[test]
    fn prune_is_idempotent_and_missing_roots_are_harmless() {
        let tree = tempfile::tempdir().expect("tree");
        let old = tree.path().join("old.jsonl");
        write_session_file(&old, "old");
        let now = std::time::UNIX_EPOCH + Duration::from_secs(PRUNE_NOW_EPOCH);
        set_modified_epoch(&old, OLD_EPOCH);
        let missing = tree.path().join("does-not-exist");

        assert_eq!(
            prune_expired_sessions(&[missing], now, PRUNE_TTL, &[], &[]),
            0,
            "missing root is a no-op"
        );
        assert_eq!(
            prune_expired_sessions(&[tree.path().to_path_buf()], now, PRUNE_TTL, &[], &[]),
            1
        );
        assert!(
            !old.exists(),
            "expired file must be gone after the first pass"
        );
        assert_eq!(
            prune_expired_sessions(&[tree.path().to_path_buf()], now, PRUNE_TTL, &[], &[]),
            0,
            "second pass deletes nothing"
        );
    }

    #[test]
    #[cfg(unix)]
    fn prune_never_follows_or_deletes_symlinks() {
        use std::os::unix::fs::symlink;

        let tree = tempfile::tempdir().expect("tree");
        let target = tree.path().join("a-target.jsonl");
        write_session_file(&target, "target");
        // The bounded capability enumeration fails closed when any entry is a
        // symlink: it never follows or deletes that link, and it does not prune
        // sibling files from a partially validated directory snapshot.
        let link = tree.path().join("z-link.jsonl");
        symlink(&target, &link).expect("symlink");
        let now = std::time::UNIX_EPOCH + Duration::from_secs(PRUNE_NOW_EPOCH);
        set_modified_epoch(&target, OLD_EPOCH);

        let deleted =
            prune_expired_sessions(&[tree.path().to_path_buf()], now, PRUNE_TTL, &[], &[]);
        assert_eq!(deleted, 0);
        assert!(
            target.exists(),
            "a symlink-contaminated directory is left untouched"
        );
        // `Path::exists` follows the link, so probe the link itself.
        assert!(
            fs::symlink_metadata(&link).is_ok(),
            "the symlink entry itself is never deleted"
        );
    }

    #[test]
    fn prune_skips_future_mtimes() {
        let tree = tempfile::tempdir().expect("tree");
        let future = tree.path().join("future.jsonl");
        write_session_file(&future, "future");
        let now = std::time::UNIX_EPOCH + Duration::from_secs(PRUNE_NOW_EPOCH);
        set_modified_epoch(&future, PRUNE_NOW_EPOCH + 60 * 60); // 1 h in the future

        let deleted =
            prune_expired_sessions(&[tree.path().to_path_buf()], now, PRUNE_TTL, &[], &[]);
        assert_eq!(
            deleted, 0,
            "clock-skewed future mtimes must never be pruned"
        );
        assert!(future.exists());
    }

    #[test]
    fn prune_never_removes_live_recorder_directory() {
        let tree = tempfile::tempdir().expect("native tree");
        let live_dir = tree.path().join("--live--");
        let stale_dir = tree.path().join("--stale--");
        fs::create_dir_all(&live_dir).expect("live per-cwd dir");
        fs::create_dir_all(&stale_dir).expect("stale per-cwd dir");
        let now = std::time::UNIX_EPOCH + Duration::from_secs(PRUNE_NOW_EPOCH);

        // A just-started recorder holds its header in memory, so its per-cwd
        // directory is EMPTY on disk at startup-prune time. The live run's
        // directory must survive the prune even though it holds no file yet;
        // other empty dirs are still best-effort removed.
        let deleted = prune_expired_sessions(
            &[tree.path().to_path_buf()],
            now,
            PRUNE_TTL,
            &[],
            &[live_dir.clone()],
        );
        assert_eq!(deleted, 0);
        assert!(
            live_dir.exists(),
            "live recorder directory must never be pruned while empty"
        );
        assert!(
            !stale_dir.exists(),
            "empty stale dir is still best-effort removed"
        );
    }

    #[test]
    fn persist_recreates_directory_removed_by_prune() {
        // Regular flush path (`persist`): the auto-id recorder starts with no
        // file on disk; a TTL prune can remove its (empty) directory before
        // the first assistant persist. The first persist must recreate it.
        let tree = tempfile::tempdir().expect("session tree");
        let directory = tree.path().join("regular-session");
        let recorder = recorder_in_retained_directory(&directory);
        assert!(
            !recorder.path().exists(),
            "auto-id recorder starts unwritten"
        );
        let now = std::time::UNIX_EPOCH + Duration::from_secs(PRUNE_NOW_EPOCH);
        assert_eq!(
            prune_expired_sessions(&[tree.path().to_path_buf()], now, PRUNE_TTL, &[], &[],),
            0,
        );
        assert!(
            !directory.exists(),
            "prune removes the empty session directory"
        );
        recorder.inner.lock().lease = None;
        recorder
            .persist_now()
            .expect("persist must recreate the removed directory");
        assert!(
            recorder.path().exists(),
            "first persist recreates the session directory"
        );
        assert!(
            load_session_tree(recorder.path()).is_ok(),
            "recreated file holds a readable session"
        );

        // Durable path (`persist_durable`, the goal journal's first write):
        // same self-healing, exercised through the goal's durable append.
        let tree = tempfile::tempdir().expect("durable session tree");
        let directory = tree.path().join("durable-session");
        let recorder = recorder_in_retained_directory(&directory);
        let now = std::time::UNIX_EPOCH + Duration::from_secs(PRUNE_NOW_EPOCH);
        assert_eq!(
            prune_expired_sessions(&[tree.path().to_path_buf()], now, PRUNE_TTL, &[], &[],),
            0,
        );
        assert!(
            !directory.exists(),
            "prune removes the empty durable directory"
        );
        recorder.inner.lock().lease = None;
        recorder
            .persist_now_durable()
            .expect("durable persist must recreate the removed directory");
        assert!(
            recorder.path().exists(),
            "first durable persist recreates the session directory"
        );
        assert!(
            load_session_tree(recorder.path()).is_ok(),
            "recreated durable file holds a readable session"
        );
    }

    #[test]
    fn native_sessions_root_matches_catalog_native_root() {
        // Both resolve PI_CODING_AGENT_DIR before RPI_HOME and the standard
        // home directory; when the environment can't yield a home the catalog
        // comparison is skipped rather than made flaky.
        let Ok(catalog) = crate::SessionCatalog::from_env() else {
            return;
        };
        let catalog_root = catalog.root_for(crate::SessionSourceKind::NativePi).path;
        assert_eq!(native_sessions_root(), catalog_root);
    }

    #[test]
    fn rewind_truncates_archives_tail_and_rebuilds_recorder_state() {
        let directory = tempfile::tempdir().expect("directory");
        let recorder = start_session_in(
            directory.path(),
            None,
            None,
            Some(directory.path()),
            Some("rewind-store"),
            None,
        )
        .expect("start session");
        let mut ids = Vec::new();
        for text in ["one", "two", "three", "four"] {
            ids.push(
                recorder
                    .record_message(&Message::user_text(text, 0))
                    .expect("record message"),
            );
        }
        recorder.persist_now().expect("persist");
        let path = recorder.path();
        assert_eq!(
            load_session_tree(&path).expect("load before").entries.len(),
            4
        );

        let outcome = recorder.rewind_to(2).expect("rewind");
        assert_eq!(outcome.retained_entries, 2);
        assert_eq!(outcome.dropped_entries, 2);
        assert!(outcome.archive_path.exists(), "archive sidecar must exist");

        // The session file is truncated to the retained records.
        let after = load_session_tree(&path).expect("load after");
        assert_eq!(after.entries.len(), 2);
        assert_eq!(after.entries[0].id, ids[0]);
        assert_eq!(after.entries[1].id, ids[1]);
        assert_eq!(after.leaf_id.as_deref(), Some(ids[1].as_str()));

        // The archive holds the dropped tail with the same record
        // serialization (plain JSONL, no header).
        let archive = fs::read_to_string(&outcome.archive_path).expect("read archive");
        let tail_ids = archive
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter_map(|value| value.get("id").and_then(Value::as_str).map(str::to_owned))
            .collect::<Vec<_>>();
        assert_eq!(tail_ids, vec![ids[2].clone(), ids[3].clone()]);

        // The recorder stays appendable from the new leaf.
        let next = recorder
            .record_message(&Message::user_text("five", 0))
            .expect("append after rewind");
        let tree = recorder.tree().expect("tree after rewind");
        let appended = tree
            .entries
            .iter()
            .find(|entry| entry.id == next)
            .expect("appended entry");
        assert_eq!(appended.parent_id.as_deref(), Some(ids[1].as_str()));
        assert_eq!(recorder.last_entry_id().as_deref(), Some(next.as_str()));
    }

    #[test]
    fn rewind_refuses_past_first_entry_and_at_end() {
        let directory = tempfile::tempdir().expect("directory");
        let recorder = start_session_in(
            directory.path(),
            None,
            None,
            Some(directory.path()),
            Some("rewind-bounds"),
            None,
        )
        .expect("start session");
        recorder
            .record_message(&Message::user_text("only", 0))
            .expect("record");
        recorder.persist_now().expect("persist");

        let error = recorder
            .rewind_to(0)
            .expect_err("rewinding past the first entry must be refused");
        assert!(format!("{error:#}").contains("past the first entry"));
        let error = recorder
            .rewind_to(1)
            .expect_err("rewinding to the end is a no-op and must be refused");
        assert!(format!("{error:#}").contains("nothing to rewind"));

        // An empty (header-only) session has nothing to rewind either.
        let empty = start_session_in(
            directory.path(),
            None,
            None,
            Some(directory.path()),
            Some("rewind-empty"),
            None,
        )
        .expect("empty session");
        empty.persist_now().expect("persist");
        let error = empty
            .rewind_to(1)
            .expect_err("empty session must refuse rewinds");
        assert!(format!("{error:#}").contains("nothing to rewind"));
    }

    #[test]
    fn record_checkpoint_marks_position_without_joining_the_chain() {
        let directory = tempfile::tempdir().expect("directory");
        let recorder = start_session_in(
            directory.path(),
            None,
            None,
            Some(directory.path()),
            Some("checkpoint-store"),
            None,
        )
        .expect("start session");
        let first = recorder
            .record_message(&Message::user_text("one", 0))
            .expect("record one");
        let second = recorder
            .record_message(&Message::user_text("two", 0))
            .expect("record two");
        recorder.persist_now().expect("persist");

        let marker = recorder.record_checkpoint("mid").expect("mark checkpoint");
        assert_eq!(
            recorder.last_entry_id().as_deref(),
            Some(second.as_str()),
            "checkpoint must not become the leaf"
        );

        let tree = recorder.tree().expect("tree");
        let checkpoint = tree
            .entries
            .iter()
            .find(|entry| entry.id == marker)
            .expect("checkpoint entry");
        assert_eq!(checkpoint.entry_type, "checkpoint");
        assert_eq!(checkpoint.name.as_deref(), Some("mid"));
        assert_eq!(checkpoint.target_id.as_deref(), Some(second.as_str()));

        // A fresh file load also treats the marker as a side record, not the
        // active leaf.
        let loaded = load_session_tree(recorder.path()).expect("fresh load");
        assert_eq!(loaded.leaf_id.as_deref(), Some(second.as_str()));

        // The next append parents from the marked entry, never the marker.
        let third = recorder
            .record_message(&Message::user_text("three", 0))
            .expect("record three");
        let tree = recorder.tree().expect("tree after append");
        let appended = tree
            .entries
            .iter()
            .find(|entry| entry.id == third)
            .expect("appended entry");
        assert_eq!(appended.parent_id.as_deref(), Some(second.as_str()));

        // Re-marking a name appends a second marker (newest wins on resolve).
        recorder.record_checkpoint("mid").expect("re-mark");
        let tree = recorder.tree().expect("tree after re-mark");
        let markers = tree
            .entries
            .iter()
            .filter(|entry| {
                entry.entry_type == "checkpoint" && entry.name.as_deref() == Some("mid")
            })
            .count();
        assert_eq!(markers, 2);
    }

    #[test]
    fn checkpoint_names_are_single_words_never_numeric_and_require_entries() {
        let directory = tempfile::tempdir().expect("directory");
        let recorder = start_session_in(
            directory.path(),
            None,
            None,
            Some(directory.path()),
            Some("checkpoint-names"),
            None,
        )
        .expect("start session");
        recorder
            .record_message(&Message::user_text("one", 0))
            .expect("record");
        for name in ["", "   ", "two words", "7"] {
            recorder
                .record_checkpoint(name)
                .expect_err("invalid checkpoint name must be refused");
        }
        recorder
            .record_checkpoint("ok-name")
            .expect("single word name is accepted");

        // An empty session cannot checkpoint: there is no position to mark.
        let empty = start_session_in(
            directory.path(),
            None,
            None,
            Some(directory.path()),
            Some("checkpoint-empty"),
            None,
        )
        .expect("empty session");
        empty.persist_now().expect("persist");
        let error = empty
            .record_checkpoint("mark")
            .expect_err("empty session cannot checkpoint");
        assert!(format!("{error:#}").contains("empty"));
    }

    fn archive_fixture() -> (tempfile::TempDir, tempfile::TempDir, PathBuf, PathBuf) {
        let agent = tempfile::tempdir().expect("agent base");
        let cwd = tempfile::tempdir().expect("cwd");
        fs::create_dir(cwd.path().join(".git")).expect("git anchor");
        let session_dir = agent.path().join("sessions").join("--project--");
        let recorder = start_session_in(
            cwd.path(),
            None,
            None,
            Some(&session_dir),
            Some("archive-session"),
            None,
        )
        .expect("start archive fixture");
        recorder
            .record_message(&Message::user_text("archive me", 0))
            .expect("record archive fixture");
        recorder.persist_now().expect("persist archive fixture");
        let session_path = recorder.path();
        recorder.close().expect("close archive fixture");
        (agent, cwd, session_dir, session_path)
    }

    #[test]
    fn archive_roundtrip_restores_exact_regular_sidecars() {
        let (_agent, _cwd, session_dir, session_path) = archive_fixture();
        let primary_name = session_path
            .file_name()
            .expect("primary name")
            .to_string_lossy();
        let sidecars = [
            format!("{primary_name}.loops.json"),
            format!("{primary_name}.rewind-one.jsonl"),
            format!("{primary_name}.snapcompact-two.jsonl"),
        ];
        let unrelated = session_dir.join(format!("{primary_name}.rewind-.jsonl"));
        fs::write(&unrelated, b"unrelated").expect("unrelated sibling");
        let primary_bytes = fs::read(&session_path).expect("primary bytes");
        for (index, name) in sidecars.iter().enumerate() {
            fs::write(session_dir.join(name), format!("sidecar-{index}")).expect("write sidecar");
        }

        let outcome = archive_saved_session(&session_path).expect("archive session");
        assert_eq!(outcome.session_id(), "archive-session");
        assert_eq!(outcome.original_path(), session_path.as_path());
        assert!(!session_path.exists());
        assert!(unrelated.exists(), "non-matching sibling remains untouched");
        for name in &sidecars {
            assert!(!session_dir.join(name).exists());
        }

        restore_archived_session(&outcome).expect("restore session");
        assert_eq!(
            fs::read(&session_path).expect("restored primary"),
            primary_bytes
        );
        for (index, name) in sidecars.iter().enumerate() {
            assert_eq!(
                fs::read(session_dir.join(name)).expect("restored sidecar"),
                format!("sidecar-{index}").as_bytes()
            );
        }
    }

    #[test]
    fn archive_uses_repo_digest_directory_and_never_overwrites_collision() {
        let (agent, cwd, _session_dir, session_path) = archive_fixture();
        let archive_dir = agent
            .path()
            .join("sessions-archived")
            .join(crate::memory::repo_namespace(cwd.path()));
        fs::create_dir_all(&archive_dir).expect("archive directory");
        let original_name = session_path.file_name().expect("session name");
        let collision = archive_dir.join(original_name);
        fs::write(&collision, b"existing archive").expect("collision file");

        let outcome = archive_saved_session(&session_path).expect("archive collision");
        assert_eq!(outcome.archive_dir(), archive_dir.as_path());
        assert_eq!(outcome.archive_dir().file_name().expect("digest").len(), 32);
        assert_eq!(
            fs::read(&collision).expect("collision preserved"),
            b"existing archive"
        );
        assert!(
            outcome.members[0]
                .archive_name
                .to_string_lossy()
                .ends_with(".1")
        );
    }

    #[cfg(unix)]
    #[test]
    fn archive_rejects_primary_symlink_and_ignores_sidecar_symlink() {
        use std::os::unix::fs::symlink;

        let (_agent, _cwd, session_dir, session_path) = archive_fixture();
        let outside = tempfile::tempdir().expect("outside");
        let outside_file = outside.path().join("outside.jsonl");
        fs::write(&outside_file, b"outside").expect("outside file");
        let primary_alias = session_dir.join("alias.jsonl");
        symlink(&outside_file, &primary_alias).expect("primary symlink");
        assert!(archive_saved_session(&primary_alias).is_err());
        assert_eq!(
            fs::read(&outside_file).expect("outside preserved"),
            b"outside"
        );

        let primary_name = session_path
            .file_name()
            .expect("primary name")
            .to_string_lossy();
        let sidecar_alias = session_dir.join(format!("{primary_name}.rewind-link.jsonl"));
        symlink(&outside_file, &sidecar_alias).expect("sidecar symlink");
        let outcome = archive_saved_session(&session_path).expect("archive regular members");
        assert!(
            sidecar_alias.is_symlink(),
            "sidecar symlink remains untouched"
        );
        restore_archived_session(&outcome).expect("restore regular members");
    }

    #[test]
    fn failed_archive_and_restore_preserve_authoritative_copy() {
        let (agent, _cwd, _session_dir, session_path) = archive_fixture();
        let original = fs::read(&session_path).expect("original bytes");
        let children = session_path
            .parent()
            .expect("session directory")
            .join("children");
        fs::create_dir_all(children.join("archive-session")).expect("durable children");
        assert!(archive_saved_session(&session_path).is_err());
        assert_eq!(fs::read(&session_path).expect("source preserved"), original);
        assert!(!agent.path().join("sessions-archived").exists());
        fs::remove_dir_all(children).expect("remove child guard");

        let outcome = archive_saved_session(&session_path).expect("archive session");
        fs::write(&session_path, b"conflict").expect("restore conflict");
        assert!(restore_archived_session(&outcome).is_err());
        assert_eq!(
            fs::read(&session_path).expect("conflict preserved"),
            b"conflict"
        );
        for member in &outcome.members {
            assert!(
                outcome
                    .archive
                    .display_root()
                    .join(&member.archive_name)
                    .exists()
            );
        }
    }

    #[test]
    fn failed_archive_copy_preserves_sources_and_cleans_created_members() {
        let (agent, cwd, session_dir, session_path) = archive_fixture();
        let primary_name = session_path
            .file_name()
            .expect("primary name")
            .to_string_lossy();
        let sidecar_name = format!("{primary_name}.loops.json");
        fs::write(session_dir.join(&sidecar_name), b"loops").expect("loops sidecar");
        let archive_dir = agent
            .path()
            .join("sessions-archived")
            .join(crate::memory::repo_namespace(cwd.path()));
        fs::create_dir_all(&archive_dir).expect("archive directory");
        for attempt in 0..=1000 {
            let collision = collision_archive_name(OsStr::new(&sidecar_name), attempt);
            fs::create_dir(archive_dir.join(collision)).expect("blocking directory collision");
        }

        assert!(archive_saved_session(&session_path).is_err());
        assert!(session_path.exists());
        assert!(session_dir.join(&sidecar_name).exists());
        assert!(
            !archive_dir
                .join(session_path.file_name().expect("primary name"))
                .exists()
        );
    }

    /// (e0 → e1 → … → e{n}) for stack-overflow regression tests.
    fn deep_linear_chain_tree(n: usize) -> SessionTree {
        let mut entries = Vec::with_capacity(n + 1);
        let mut by_id = HashMap::new();
        let mut children_by_parent: HashMap<Option<String>, Vec<usize>> = HashMap::new();

        children_by_parent.insert(None, vec![0]);
        entries.push(linear_chain_entry("e0", None));
        by_id.insert("e0".to_owned(), 0);

        for i in 1..=n {
            let parent_id = format!("e{}", i - 1);
            let id = format!("e{i}");
            let idx = entries.len();
            children_by_parent.insert(Some(parent_id.clone()), vec![idx]);
            entries.push(linear_chain_entry(&id, Some(parent_id)));
            by_id.insert(id, idx);
        }

        SessionTree {
            header: SessionHeader {
                record_type: "session".to_owned(),
                version: 3,
                id: format!("deep-chain-{n}"),
                timestamp: "2026-01-01T00:00:00.000Z".to_owned(),
                cwd: PathBuf::new(),
                parent_session: None,
            },
            entries,
            leaf_id: Some(format!("e{n}")),
            active_leaf_id: Some(format!("e{n}")),
            skipped_idless: 0,
            undecodable_messages: 0,
            by_id,
            children_by_parent,
            labels: HashMap::new(),
        }
    }

    fn linear_chain_entry(id: &str, parent_id: Option<String>) -> SessionEntry {
        SessionEntry {
            id: id.to_owned(),
            parent_id,
            entry_type: "message".to_owned(),
            timestamp: "2026-01-01T00:00:00.000Z".to_owned(),
            message: Some(Message::user_text("msg", 0)),
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
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct LegacyTreeNode<'a> {
        entry: &'a SessionEntry,
        children: Vec<LegacyTreeNode<'a>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        label: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        label_timestamp: Option<&'a str>,
    }

    fn legacy_tree_node(node: &SessionTreeNode) -> LegacyTreeNode<'_> {
        LegacyTreeNode {
            entry: &node.entry,
            children: node.children.iter().map(legacy_tree_node).collect(),
            label: node.label.as_deref(),
            label_timestamp: node.label_timestamp.as_deref(),
        }
    }

    #[test]
    fn tree_node_serialization_matches_nested_branching_contract() {
        let result = SessionTreeResult {
            tree: vec![SessionTreeNode {
                entry: linear_chain_entry("root", None),
                children: vec![
                    SessionTreeNode {
                        entry: linear_chain_entry("left", Some("root".to_owned())),
                        children: vec![],
                        label: Some("left label".to_owned()),
                        label_timestamp: Some("2026-01-01T00:00:01.000Z".to_owned()),
                    },
                    SessionTreeNode {
                        entry: linear_chain_entry("right", Some("root".to_owned())),
                        children: vec![SessionTreeNode {
                            entry: linear_chain_entry("leaf", Some("right".to_owned())),
                            children: vec![],
                            label: None,
                            label_timestamp: None,
                        }],
                        label: None,
                        label_timestamp: None,
                    },
                ],
                label: Some("root label".to_owned()),
                label_timestamp: None,
            }],
            leaf_id: Some("leaf".to_owned()),
            active_leaf_id: Some("leaf".to_owned()),
        };
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct LegacyTreeResult<'a> {
            tree: Vec<LegacyTreeNode<'a>>,
            leaf_id: Option<&'a str>,
            active_leaf_id: Option<&'a str>,
        }

        let expected = serde_json::to_string(&LegacyTreeResult {
            tree: result.tree.iter().map(legacy_tree_node).collect(),
            leaf_id: result.leaf_id.as_deref(),
            active_leaf_id: result.active_leaf_id.as_deref(),
        })
        .expect("serialize prior derived contract");
        let encoded = serde_json::to_string(&result).expect("serialize branching tree");
        assert_eq!(
            encoded, expected,
            "nested GetTree bytes must remain unchanged"
        );
        let actual: Value = serde_json::from_str(&encoded).expect("parse branching tree");
        let prior: Value = serde_json::from_str(&expected).expect("parse prior contract");
        assert_eq!(actual, prior);
    }

    /// Stack overflow regression: `SessionTree::tree()` must materialize a
    /// deeply nested (linear-chain) session tree iteratively, not recursively.
    /// A session with N=20,000 linearly-chained entries overflows the default
    /// 2 MiB Tokio worker stack under the old recursive `build`. This test
    /// runs `tree()` on a 256 KiB stack thread — small enough that a reverted
    /// recursive implementation aborts with SIGABRT, while the iterative
    /// worklist completes in constant stack.
    #[test]
    fn tree_materializes_deep_linear_chain_without_stack_overflow() {
        let tree = deep_linear_chain_tree(20_000);
        const CONSTRAINED_STACK: usize = 256 * 1024;
        let result = std::thread::Builder::new()
            .stack_size(CONSTRAINED_STACK)
            .spawn(move || {
                let nodes = tree.tree();
                drop(nodes);
            })
            .expect("constrained-stack thread")
            .join();
        assert!(
            result.is_ok(),
            "iterative tree() must not overflow {} stack",
            CONSTRAINED_STACK
        );
    }

    /// Stack overflow regression: dropping a `SessionTreeResult` with a deep
    /// `children` chain must not overflow. The custom `Drop` impl drains the
    /// forest iteratively via a heap worklist.
    #[test]
    fn tree_result_drop_does_not_overflow_on_deep_chain() {
        let tree = deep_linear_chain_tree(20_000);
        let result = tree.tree();
        const CONSTRAINED_STACK: usize = 128 * 1024;
        let join_result = std::thread::Builder::new()
            .stack_size(CONSTRAINED_STACK)
            .spawn(move || drop(result))
            .expect("constrained-stack thread")
            .join();
        assert!(
            join_result.is_ok(),
            "SessionTreeResult::drop must not overflow {} stack",
            CONSTRAINED_STACK
        );
    }

    /// Stack overflow regression: serializing a deep `SessionTreeResult` to
    /// the nested GetTree JSON contract must not overflow. Each node writes
    /// forward through one explicit worklist; the final `RawValue` validates
    /// and emits those bytes without recursive `Serialize` calls.
    #[test]
    fn tree_result_serialization_does_not_overflow_on_deep_chain() {
        const ENTRY_COUNT: usize = 20_001;
        let tree = deep_linear_chain_tree(ENTRY_COUNT - 1);
        let result = tree.tree();
        const WORKER_STACK: usize = 2 * 1024 * 1024;
        let join_result = std::thread::Builder::new()
            .stack_size(WORKER_STACK)
            .spawn(move || serde_json::to_string(&result))
            .expect("worker-stack thread")
            .join();
        assert!(
            join_result.is_ok(),
            "serialization must not overflow {} stack",
            WORKER_STACK
        );
        let encoded = join_result
            .unwrap()
            .expect("serialization must produce nested JSON");
        let raw = serde_json::value::RawValue::from_string(encoded)
            .expect("deep output must parse as one JSON value");
        let encoded = raw.get();
        assert_eq!(encoded.matches(r#""entry":"#).count(), ENTRY_COUNT);
        assert_eq!(encoded.matches(r#""children":[{"#).count(), ENTRY_COUNT - 1);
        assert_eq!(encoded.matches(r#""children":[]"#).count(), 1);

        let mut cursor = 0;
        for index in 0..ENTRY_COUNT {
            let parent = if index == 0 {
                "null".to_owned()
            } else {
                format!("\"e{}\"", index - 1)
            };
            let expected = format!(
                r#""entry":{{"type":"message","id":"e{index}","parentId":{parent},"timestamp":"#
            );
            let offset = encoded[cursor..]
                .find(&expected)
                .unwrap_or_else(|| panic!("missing ordered entry e{index}"));
            cursor += offset + expected.len();
        }
        assert!(encoded.ends_with(r#"],"leafId":"e20000","activeLeafId":"e20000"}"#));
    }

    fn fork_user_entry(id: &str, parent_id: Option<&str>, text: &str) -> SessionEntry {
        let mut entry = test_entry(id, parent_id);
        entry.message = Some(Message::user_text(text, 0));
        entry
    }

    fn chained_user_entries(count: usize, text: &str) -> Vec<SessionEntry> {
        let mut entries = Vec::with_capacity(count);
        for index in 0..count {
            let parent = if index == 0 { None } else { Some(format!("u{}", index - 1)) };
            let id = format!("u{index}");
            entries.push(fork_user_entry(&id, parent.as_deref(), &format!("{text} {index}")));
        }
        entries
    }

    #[test]
    fn fork_messages_page_bounds_large_linear_session() {
        let long_body = format!("first line {}", "x".repeat(400));
        let expected_preview: String = long_body.chars().take(FORK_MESSAGES_PREVIEW_CHARS).collect();
        let entries = chained_user_entries(200, &long_body);
        let tree = test_tree(entries, None);
        let page = tree.fork_messages_page(None, None).expect("first page");
        assert_eq!(page.messages.len(), FORK_MESSAGES_PAGE_LIMIT);
        // Newest-first: the last chained entry leads the page.
        assert_eq!(page.messages[0].entry_id, "u199");
        assert_eq!(page.messages[0].text, expected_preview);
        assert_ne!(page.messages[0].text, long_body, "preview must not carry the full body");
        assert!(page.truncated);
        assert!(page.next_cursor.is_some());
        assert_eq!(page.total, Some(200));

        let cursor = page.next_cursor.expect("next cursor");
        let older = tree
            .fork_messages_page(Some(&cursor), None)
            .expect("older page");
        let first_ids: HashSet<String> = page.messages.iter().map(|message| message.entry_id.clone()).collect();
        for message in &older.messages {
            assert!(
                !first_ids.contains(message.entry_id.as_str()),
                "pages must be disjoint: {message:?}"
            );
        }
        // Deterministic: the same cursor pages identically.
        let replay = tree.fork_messages_page(Some(&cursor), None).expect("replay");
        assert_eq!(replay.messages, older.messages);
        assert_eq!(replay.next_cursor, older.next_cursor);
        // Walking to exhaustion yields every candidate exactly once.
        let mut seen = first_ids;
        let mut cursor = Some(cursor);
        while let Some(cursor_id) = cursor {
            let next = tree
                .fork_messages_page(Some(&cursor_id), None)
                .expect("next page");
            let message_ids: Vec<String> = next.messages.iter().map(|message| message.entry_id.clone()).collect();
            let empty = message_ids.is_empty();
            for message_id in &message_ids {
                assert!(
                    seen.insert(message_id.clone()),
                    "duplicate entry across pages"
                );
            }
            cursor = next.next_cursor;
            if empty {
                break;
            }
        }
        assert_eq!(seen.len(), 200);
    }

    #[test]
    fn fork_messages_page_fails_closed_on_unknown_cursor() {
        let entries = chained_user_entries(3, "prompt");
        let tree = test_tree(entries, None);
        assert!(tree.fork_messages_page(Some("missing"), None).is_err());
        assert!(tree.fork_messages_page(Some(""), None).is_err());
        assert!(tree.fork_messages_page(None, Some(0)).is_err());
        // Cursors to assistant/tool entries are not candidates.
        assert!(tree.fork_messages_page(Some("e0"), None).is_err());
    }

    #[test]
    fn fork_messages_page_excludes_empty_and_assistant_entries() {
        let entries = vec![
            fork_user_entry("e0", Some("h"), " "),
            fork_user_entry("e1", Some("e0"), ""),
            fork_user_entry("e2", Some("e1"), "only real"),
        ];
        let tree = test_tree(entries, None);
        let page = tree.fork_messages_page(None, None).expect("page");
        assert_eq!(page.messages.len(), 1);
        assert_eq!(page.messages[0].entry_id, "e2");
        assert_eq!(page.messages[0].text, "only real");
        assert!(!page.truncated);
        assert!(page.next_cursor.is_none());
    }

    #[test]
    fn fork_messages_page_stays_under_wire_budget() {
        let long_body = format!("head {}", "b".repeat(2000));
        let entries = chained_user_entries(200, &long_body);
        let tree = test_tree(entries, None);
        let page = tree.fork_messages_page(None, None).expect("page");
        let serialized = serde_json::to_vec(&page).expect("serialize page");
        assert!(serialized.len() <= FORK_MESSAGES_PAGE_BYTES);
        assert!(page.messages.len() < 200, "byte budget must stop the page");
        assert!(page.truncated);
    }

    #[test]
    fn fork_messages_page_supports_large_ids_and_rejects_unframeable_ones() {
        let long_id = "u".repeat(64 * 1024);
        let entries = vec![
            fork_user_entry("old", Some("h"), "older"),
            fork_user_entry(&long_id, Some("old"), "newer"),
        ];
        let tree = test_tree(entries, None);
        let page = tree.fork_messages_page(None, Some(1)).expect("first page");
        assert_eq!(page.messages[0].entry_id, long_id);
        assert_eq!(page.next_cursor.as_deref(), Some(page.messages[0].entry_id.as_str()));
        let older = tree
            .fork_messages_page(page.next_cursor.as_deref(), Some(1))
            .expect("older page");
        assert_eq!(older.messages[0].entry_id, "old");

        let unframeable_id = "u".repeat(128 * 1024);
        let unframeable = test_tree(
            vec![
                fork_user_entry("older", Some("h"), "older"),
                fork_user_entry(&unframeable_id, Some("older"), "newer"),
            ],
            None,
        );
        assert!(unframeable.fork_messages_page(None, Some(1)).is_err());
    }

    fn read_header(path: &Path) -> serde_json::Value {
        let file = fs::read_to_string(path).expect("read session file");
        serde_json::from_str(file.lines().next().expect("header line")).expect("header json")
    }

    #[test]
    fn unknown_session_kind_header_value_is_ignored() {
        for kind in SessionKind::ALL {
            assert_eq!(
                SessionKind::from_header_value(kind.header_value()),
                Some(kind)
            );
            assert_eq!(
                kind.is_workflow_internal(),
                !matches!(kind, SessionKind::User)
            );
        }
        assert_eq!(SessionKind::from_header_value(""), None);
        assert_eq!(SessionKind::from_header_value("workflow"), None);
    }

    #[test]
    fn workflow_session_header_stamps_authoritative_kind() {
        let cwd = tempfile::tempdir().expect("cwd");
        let sessions = tempfile::tempdir().expect("sessions");
        let recorder = start_session_in_with_kind(
            cwd.path(),
            None,
            None,
            Some(sessions.path()),
            None,
            None,
            Some(SessionKind::WorkflowSupervisor),
        )
        .expect("workflow session");
        recorder.persist_now().expect("persist header");
        let header = read_header(&recorder.path());
        assert_eq!(header["sessionKind"], "workflowSupervisor");
    }

    #[test]
    fn user_session_header_omits_session_kind_field() {
        let cwd = tempfile::tempdir().expect("cwd");
        let sessions = tempfile::tempdir().expect("sessions");
        let recorder = start_session_in(
            cwd.path(),
            None,
            None,
            Some(sessions.path()),
            None,
            None,
        )
        .expect("user session");
        recorder.persist_now().expect("persist header");
        let header = read_header(&recorder.path());
        assert!(header.get("sessionKind").is_none());
    }

    #[test]
    fn durable_workflow_worker_header_stamps_session_kind() {
        let cwd = tempfile::tempdir().expect("cwd");
        let child_root = tempfile::tempdir().expect("child root");
        let parent = child_root.path().join("parent.jsonl");
        let recorder = start_durable_child_session_in_with_kind(
            cwd.path(),
            None,
            None,
            child_root.path(),
            None,
            &parent,
            Some(SessionKind::WorkflowWorker),
        )
        .expect("worker session");
        let header = read_header(&recorder.path());
        assert_eq!(header["sessionKind"], "workflowWorker");
    }

    #[test]
    fn user_durable_child_header_omits_session_kind_field() {
        let cwd = tempfile::tempdir().expect("cwd");
        let child_root = tempfile::tempdir().expect("child root");
        let parent = child_root.path().join("parent.jsonl");
        let recorder = start_durable_child_session_in(
            cwd.path(),
            None,
            None,
            child_root.path(),
            None,
            &parent,
        )
        .expect("durable child session");
        let header = read_header(&recorder.path());
        assert!(header.get("sessionKind").is_none());
    }
}
