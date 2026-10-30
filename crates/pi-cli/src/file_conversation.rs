//! File-conversation engine: a bounded per-dispatcher collection of read-only
//! side-chat controllers, each conversation bound to a client-generated id, a
//! resolved workspace path, and captured owner facts. No internal lock: the
//! RPC dispatcher owns the collection mutex and serializes every call.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use pi_coding::{Application, SessionFileIdentity};
use serde_json::{Value, json};

use crate::file_explorer::{self, FileExplorerError};
use crate::side_chat::SideChatController;

/// Whole-collection cap matching the client tab cap; superseded entries keep
/// their slot until explicitly retired or closed.
pub(crate) const FILE_CONVERSATION_MAX_OPEN: usize = 8;

pub(crate) const FILE_CONVERSATION_PROMPT_MAX_BYTES: usize = 8192;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RecordedSessionIdentity {
    pub(crate) recorder_id: String,
    pub(crate) session_path: PathBuf,
    pub(crate) file_identity: SessionFileIdentity,
}

/// Captured owner facts. Recorder presence (`Session::recorder_info`) is the
/// authoritative unrecorded distinction: `None` stores no identity (a same-cwd
/// replacement of an unrecorded session is then undetectable — those
/// replacement paths must retire/shutdown explicitly), and a present recorder
/// makes `recording_writer_info` errors propagate, never swallowed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FileConversationOwner {
    pub(crate) runtime_epoch: u64,
    pub(crate) recording: Option<RecordedSessionIdentity>,
    pub(crate) cwd: PathBuf,
}

impl FileConversationOwner {
    pub(crate) fn capture(app: &Application) -> Result<Self, FileConversationError> {
        let session = app.session();
        let recording = match session.recorder_info() {
            None => None,
            Some(_) => {
                let (recorder_id, session_path, file_identity) =
                    session.recording_writer_info().map_err(|error| {
                        FileConversationError::Owner(format!(
                            "failed to capture recorded-session identity: {error:#}"
                        ))
                    })?;
                Some(RecordedSessionIdentity { recorder_id, session_path, file_identity })
            }
        };
        Ok(Self {
            runtime_epoch: app.runtime_epoch(),
            recording,
            cwd: session.cwd().to_path_buf(),
        })
    }
}

/// 1-based inclusive source-file line range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FileConversationLineRange {
    pub(crate) start_line: usize,
    pub(crate) end_line: usize,
}

impl FileConversationLineRange {
    pub(crate) const fn line_count(self) -> usize {
        self.end_line - self.start_line + 1
    }

    pub(crate) fn to_wire_json(self) -> Value {
        json!({ "startLine": self.start_line, "endLine": self.end_line })
    }
}

/// `requestedRange` is absent without a selection; `includedRange` is an
/// explicit null for an empty inclusion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FileConversationContext {
    pub(crate) requested_range: Option<FileConversationLineRange>,
    pub(crate) included_range: Option<FileConversationLineRange>,
    pub(crate) truncated: bool,
}

impl FileConversationContext {
    pub(crate) fn to_wire_json(&self) -> Value {
        let mut object = serde_json::Map::new();
        if let Some(range) = self.requested_range {
            object.insert("requestedRange".to_owned(), range.to_wire_json());
        }
        object.insert(
            "includedRange".to_owned(),
            self.included_range
                .map(FileConversationLineRange::to_wire_json)
                .unwrap_or(Value::Null),
        );
        object.insert("truncated".to_owned(), Value::Bool(self.truncated));
        Value::Object(object)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct FileConversationSnapshot {
    pub(crate) conversation_id: String,
    pub(crate) path: String,
    pub(crate) streaming: bool,
    pub(crate) status: String,
    pub(crate) entries: Vec<Value>,
    pub(crate) context: Option<FileConversationContext>,
}

impl FileConversationSnapshot {
    pub(crate) fn to_wire_json(&self) -> Value {
        let mut object = json!({
            "conversationId": self.conversation_id,
            "path": self.path,
            "streaming": self.streaming,
            "status": self.status,
            "entries": self.entries,
        });
        if let Some(context) = &self.context {
            object["context"] = context.to_wire_json();
        }
        object
    }
}

/// Prompt acknowledgement; `busy` appears on the wire only when `true`.
#[derive(Clone, Debug)]
pub(crate) struct FileConversationPromptAck {
    pub(crate) snapshot: FileConversationSnapshot,
    pub(crate) accepted: bool,
    pub(crate) busy: bool,
}

impl FileConversationPromptAck {
    pub(crate) fn to_wire_json(&self) -> Value {
        let mut object = self.snapshot.to_wire_json();
        object["accepted"] = Value::Bool(self.accepted);
        if self.busy {
            object["busy"] = Value::Bool(true);
        }
        object
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum FileConversationError {
    Capacity { limit: usize },
    Superseded,
    NotFound,
    BindingMismatch,
    Owner(String),
    Open(String),
    PromptEmpty,
    PromptTooLarge { len_bytes: usize, limit: usize },
    SelectionInvalid { start_line: usize, end_line: usize },
    SelectionTooLarge { requested_lines: usize, limit: usize },
    SelectionOutOfRange {
        start_line: usize,
        end_line: usize,
        total_lines: usize,
    },
    /// `code` carries the typed file-explorer preview code when present.
    Read {
        message: String,
        code: Option<FileExplorerError>,
    },
}

impl FileConversationError {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::Capacity { .. } => "file_conversation_capacity",
            Self::Superseded => "file_conversation_superseded",
            Self::NotFound => "file_conversation_not_found",
            Self::BindingMismatch => "file_conversation_binding_mismatch",
            Self::Owner(_) => "file_conversation_owner_capture_failed",
            Self::Open(_) => "file_conversation_open_failed",
            Self::PromptEmpty => "file_conversation_prompt_empty",
            Self::PromptTooLarge { .. } => "file_conversation_prompt_too_large",
            Self::SelectionInvalid { .. } => "file_conversation_selection_invalid",
            Self::SelectionTooLarge { .. } => "file_conversation_selection_too_large",
            Self::SelectionOutOfRange { .. } => "file_conversation_selection_out_of_range",
            Self::Read { .. } => "file_conversation_read_failed",
        }
    }

    pub(crate) fn message(&self) -> String {
        match self {
            Self::Capacity { limit } => {
                format!("file conversation limit reached ({limit} open); close one before opening another")
            }
            Self::Superseded => {
                "file conversation owner changed (session or workspace replaced); conversation closed"
                    .to_owned()
            }
            Self::NotFound => "no file conversation with this conversationId".to_owned(),
            Self::BindingMismatch => {
                "conversationId is already bound to a different path and cannot be rebound"
                    .to_owned()
            }
            Self::Owner(error) => format!("failed to capture conversation owner: {error}"),
            Self::Open(error) => format!("failed to fork file conversation controller: {error}"),
            Self::PromptEmpty => "file conversation prompt is empty".to_owned(),
            Self::PromptTooLarge { len_bytes, limit } => {
                format!("file conversation prompt is {len_bytes} bytes, exceeding the {limit}-byte limit")
            }
            Self::SelectionInvalid {
                start_line,
                end_line,
            } => {
                format!("invalid selection: lines {start_line}-{end_line} must be 1-based with startLine <= endLine")
            }
            Self::SelectionTooLarge {
                requested_lines,
                limit,
            } => {
                format!("selection of {requested_lines} lines cannot be included in full within the context bounds (hard maximum {limit} lines or the 256KiB page ceiling); narrow the selection")
            }
            Self::SelectionOutOfRange {
                start_line,
                end_line,
                total_lines,
            } => {
                format!("selection lines {start_line}-{end_line} exceed the file total of {total_lines} lines")
            }
            Self::Read { message, .. } => message.clone(),
        }
    }

    pub(crate) fn failure_data(&self, conversation_id: &str) -> Value {
        let mut data = json!({
            "conversationId": conversation_id,
            "code": self.code(),
        });
        match self {
            Self::Capacity { limit } => data["limit"] = json!(limit),
            Self::PromptTooLarge { len_bytes, limit } => {
                data["lenBytes"] = json!(len_bytes);
                data["limit"] = json!(limit);
            }
            Self::SelectionTooLarge {
                requested_lines,
                limit,
            } => {
                data["requestedLines"] = json!(requested_lines);
                data["limit"] = json!(limit);
            }
            Self::SelectionOutOfRange {
                start_line,
                end_line,
                total_lines,
            } => {
                data["startLine"] = json!(start_line);
                data["endLine"] = json!(end_line);
                data["totalLines"] = json!(total_lines);
            }
            Self::SelectionInvalid {
                start_line,
                end_line,
            } => {
                data["startLine"] = json!(start_line);
                data["endLine"] = json!(end_line);
            }
            Self::Read { code, .. } => {
                if let Some(typed) = code
                    && let Ok(typed) = serde_json::to_value(typed)
                {
                    data["code"] = typed;
                }
            }
            Self::Superseded
            | Self::NotFound
            | Self::BindingMismatch
            | Self::Owner(_)
            | Self::Open(_)
            | Self::PromptEmpty => {}
        }
        data
    }
}

struct FileConversation {
    path: String,
    owner: FileConversationOwner,
    controller: SideChatController,
    context: Option<FileConversationContext>,
}

impl FileConversation {
    fn snapshot(&self, conversation_id: &str) -> FileConversationSnapshot {
        FileConversationSnapshot {
            conversation_id: conversation_id.to_owned(),
            path: self.path.clone(),
            streaming: self.controller.is_streaming(),
            status: self.controller.status().to_owned(),
            entries: self
                .controller
                .entries()
                .iter()
                .map(crate::side_chat::SideChatEntry::to_wire_json)
                .collect(),
            context: self.context,
        }
    }
}

#[derive(Default)]
pub(crate) struct FileConversationEngine {
    conversations: HashMap<String, FileConversation>,
}

impl FileConversationEngine {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Capacity first (no read/fork), then strict binding via the resolved
    /// preview path — no basename inference; ids are immutable.
    pub(crate) async fn open(
        &mut self,
        app: &Application,
        conversation_id: &str,
        path: &str,
    ) -> Result<FileConversationSnapshot, FileConversationError> {
        let current = FileConversationOwner::capture(app)?;
        if let Some(conversation) = self.conversations.get(conversation_id) {
            if conversation.owner != current {
                let mut stale =
                    self.conversations.remove(conversation_id).expect("id checked present");
                stale.controller.shutdown().await;
                return Err(FileConversationError::Superseded);
            }
        }
        if let Some(bound_path) =
            self.conversations.get(conversation_id).map(|conversation| conversation.path.clone())
        {
            let workspace_root = app.session().cwd().to_path_buf();
            let probe = read_strict(app, &workspace_root, path, 0, None)?;
            if bound_path == probe.path {
                let mut live =
                    self.conversations.get_mut(conversation_id).expect("id checked present");
                live.controller.poll_events();
                return Ok(live.snapshot(conversation_id));
            }
            return Err(FileConversationError::BindingMismatch);
        }
        if self.conversations.len() >= FILE_CONVERSATION_MAX_OPEN {
            return Err(FileConversationError::Capacity {
                limit: FILE_CONVERSATION_MAX_OPEN,
            });
        }
        let workspace_root = app.session().cwd().to_path_buf();
        let page = read_strict(app, &workspace_root, path, 0, None)?;
        let controller = SideChatController::fork_from(app)
            .await
            .map_err(|error| FileConversationError::Open(format!("{error:#}")))?;
        let conversation = FileConversation {
            path: page.path,
            owner: current,
            controller,
            context: None,
        };
        self.conversations
            .insert(conversation_id.to_owned(), conversation);
        Ok(self
            .conversations
            .get(conversation_id)
            .expect("just inserted")
            .snapshot(conversation_id))
    }

    pub(crate) async fn prompt(
        &mut self,
        app: &Application,
        conversation_id: &str,
        message: &str,
        selection: Option<FileConversationLineRange>,
    ) -> Result<FileConversationPromptAck, FileConversationError> {
        self.check_live(app, conversation_id).await?;
        let conversation =
            self.conversations.get_mut(conversation_id).expect("present after live check");
        conversation.controller.poll_events();
        if conversation.controller.is_streaming() {
            return Ok(FileConversationPromptAck {
                snapshot: conversation.snapshot(conversation_id),
                accepted: false,
                busy: true,
            });
        }
        if message.trim().is_empty() {
            return Err(FileConversationError::PromptEmpty);
        }
        if message.len() > FILE_CONVERSATION_PROMPT_MAX_BYTES {
            return Err(FileConversationError::PromptTooLarge {
                len_bytes: message.len(),
                limit: FILE_CONVERSATION_PROMPT_MAX_BYTES,
            });
        }
        let workspace_root = app.session().cwd().to_path_buf();
        let stored_path = conversation.path.clone();
        let (lines, context) = read_context(app, &workspace_root, &stored_path, selection)?;
        let composed = compose_prompt(message, &stored_path, &lines, &context);
        let conversation =
            self.conversations.get_mut(conversation_id).expect("present after live check");
        conversation.controller.submit_displayed_prompt(
            composed,
            Vec::new(),
            Some(message.trim()),
        );
        conversation.context = Some(context);
        Ok(FileConversationPromptAck {
            snapshot: conversation.snapshot(conversation_id),
            accepted: true,
            busy: false,
        })
    }

    pub(crate) async fn snapshot(
        &mut self,
        app: &Application,
        conversation_id: &str,
    ) -> Result<FileConversationSnapshot, FileConversationError> {
        self.check_live(app, conversation_id).await?;
        let conversation =
            self.conversations.get_mut(conversation_id).expect("present after live check");
        conversation.controller.poll_events();
        Ok(conversation.snapshot(conversation_id))
    }

    pub(crate) async fn abort(
        &mut self,
        app: &Application,
        conversation_id: &str,
    ) -> Result<FileConversationSnapshot, FileConversationError> {
        self.check_live(app, conversation_id).await?;
        let conversation =
            self.conversations.get_mut(conversation_id).expect("present after live check");
        conversation.controller.poll_events();
        conversation.controller.abort_streaming().await;
        conversation.controller.poll_events();
        Ok(conversation.snapshot(conversation_id))
    }

    pub(crate) async fn close(
        &mut self,
        app: &Application,
        conversation_id: &str,
    ) -> Result<Value, FileConversationError> {
        let current = FileConversationOwner::capture(app)?;
        match self.conversations.remove(conversation_id) {
            None => Err(FileConversationError::NotFound),
            Some(mut conversation) => {
                if conversation.owner != current {
                    conversation.controller.shutdown().await;
                    return Err(FileConversationError::Superseded);
                }
                conversation.controller.shutdown().await;
                Ok(json!({ "conversationId": conversation_id, "closed": true }))
            }
        }
    }

    pub(crate) async fn is_streaming(
        &mut self,
        app: &Application,
        conversation_id: &str,
    ) -> Result<bool, FileConversationError> {
        self.check_live(app, conversation_id).await?;
        let conversation =
            self.conversations.get_mut(conversation_id).expect("present after live check");
        conversation.controller.poll_events();
        Ok(conversation.controller.is_streaming())
    }

    /// Polls every live controller before testing flags (terminal completion
    /// is event-driven); capture errors propagate instead of masking as
    /// not-busy. Call with the collection mutex already held.
    pub(crate) async fn is_busy(
        &mut self,
        app: &Application,
    ) -> Result<bool, FileConversationError> {
        let current = FileConversationOwner::capture(app)?;
        self.retire_stale(&current).await;
        let mut busy = false;
        for conversation in self.conversations.values_mut() {
            conversation.controller.poll_events();
            busy |= conversation.controller.is_streaming();
        }
        Ok(busy)
    }

    /// See [`FileConversationOwner`]: a same-cwd replacement of an unrecorded
    /// session is undetectable here; those paths must call [`Self::shutdown`]
    /// (or per-id close) BEFORE swapping the live owner.
    pub(crate) async fn retire_superseded(
        &mut self,
        app: &Application,
    ) -> Result<Vec<String>, FileConversationError> {
        let current = FileConversationOwner::capture(app)?;
        Ok(self.retire_stale(&current).await)
    }

    pub(crate) async fn shutdown(&mut self) -> usize {
        let count = self.conversations.len();
        for (_, mut conversation) in self.conversations.drain() {
            conversation.controller.shutdown().await;
        }
        count
    }

    async fn retire_stale(&mut self, current: &FileConversationOwner) -> Vec<String> {
        let stale_ids: Vec<String> = self
            .conversations
            .iter()
            .filter(|(_, conversation)| conversation.owner != *current)
            .map(|(id, _)| id.clone())
            .collect();
        for id in &stale_ids {
            if let Some(mut stale) = self.conversations.remove(id) {
                stale.controller.shutdown().await;
            }
        }
        stale_ids
    }

    /// Superseded -> shut down the exact controller, remove, answer
    /// [`FileConversationError::Superseded`]; unknown id -> `NotFound`.
    async fn check_live(
        &mut self,
        app: &Application,
        conversation_id: &str,
    ) -> Result<(), FileConversationError> {
        let current = FileConversationOwner::capture(app)?;
        match self.conversations.get(conversation_id) {
            None => Err(FileConversationError::NotFound),
            Some(conversation) if conversation.owner != current => {
                let mut stale =
                    self.conversations.remove(conversation_id).expect("id checked present");
                stale.controller.shutdown().await;
                Err(FileConversationError::Superseded)
            }
            Some(_) => Ok(()),
        }
    }
}

/// Strict preview authority: `read_page` only, no basename fallback.
fn read_strict(
    app: &Application,
    workspace_root: &Path,
    path: &str,
    cursor: usize,
    max_lines: Option<usize>,
) -> Result<file_explorer::ReadResponse, FileConversationError> {
    file_explorer::read_page(app, workspace_root, path, cursor, max_lines).map_err(|error| {
        let code = error.downcast_ref::<FileExplorerError>().copied();
        FileConversationError::Read {
            message: format!("strict file read failed for {path}: {error:#}"),
            code,
        }
    })
}

fn read_context(
    app: &Application,
    workspace_root: &Path,
    path: &str,
    selection: Option<FileConversationLineRange>,
) -> Result<(Vec<String>, FileConversationContext), FileConversationError> {
    let Some(selection) = selection else {
        let page = read_strict(app, workspace_root, path, 0, None)?;
        let included_range = if page.lines.is_empty() {
            None
        } else {
            Some(FileConversationLineRange { start_line: 1, end_line: page.lines.len() })
        };
        let context = FileConversationContext {
            requested_range: None,
            included_range,
            truncated: page.next_cursor.is_some(),
        };
        return Ok((page.lines, context));
    };
    if selection.start_line == 0 || selection.end_line < selection.start_line {
        return Err(FileConversationError::SelectionInvalid {
            start_line: selection.start_line,
            end_line: selection.end_line,
        });
    }
    let requested = selection.line_count();
    if requested > file_explorer::MAX_PAGE_LINES {
        return Err(FileConversationError::SelectionTooLarge {
            requested_lines: requested,
            limit: file_explorer::MAX_PAGE_LINES,
        });
    }
    let page =
        read_strict(app, workspace_root, path, selection.start_line - 1, Some(requested))?;
    if page.lines.len() < requested {
        if selection.end_line > page.total_lines {
            return Err(FileConversationError::SelectionOutOfRange {
                start_line: selection.start_line,
                end_line: selection.end_line,
                total_lines: page.total_lines,
            });
        }
        return Err(FileConversationError::SelectionTooLarge {
            requested_lines: requested,
            limit: file_explorer::MAX_PAGE_LINES,
        });
    }
    let context = FileConversationContext {
        requested_range: Some(selection),
        included_range: Some(selection),
        truncated: false,
    };
    Ok((page.lines, context))
}

fn compose_prompt(
    message: &str,
    path: &str,
    lines: &[String],
    context: &FileConversationContext,
) -> String {
    let range = context
        .included_range
        .map(|range| format!("lines {}-{}", range.start_line, range.end_line))
        .unwrap_or_else(|| "no lines".to_owned());
    let truncated =
        if context.truncated { " (truncated at the context bound)" } else { "" };
    let body =
        if lines.is_empty() { "(the file has no lines)".to_owned() } else { lines.join("\n") };
    format!(
        "{message}\n\n--- FILE CONTEXT: {path} · {range}{truncated} ---\n{body}\n--- END FILE CONTEXT ---"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestApplication {
        application: pi_coding::Application,
        _agent: tempfile::TempDir,
        _session_dir: tempfile::TempDir,
    }

    async fn test_application(cwd: &Path, recorded: bool) -> TestApplication {
        pi_ai::providers::register_default_faux();
        let model = pi_ai::get_model("faux", "faux-1").expect("faux model");
        let workspace =
            pi_coding::WorkspaceRoots::new(cwd, Vec::<PathBuf>::new()).expect("workspace");
        let session =
            pi_coding::Session::new_with_additional_tools_filtered_discovery_workspace_and_uri(
                pi_coding::SessionOptions {
                    model,
                    cwd: cwd.to_path_buf(),
                    system_prompt: String::new(),
                    thinking_level: pi_agent::ThinkingLevel::Off,
                    api_key: "faux".into(),
                    compaction: None,
                    stream_options: Default::default(),
                    tools: Some(Vec::new()),
                    before_tool_call: None,
                    after_tool_call: None,
                    stream_fn: None,
                    auth_resolver: None,
                    auth_manager: None,
                },
                Vec::new(),
                pi_coding::ToolSelection::default(),
                pi_coding::ResourceDiscovery::Disabled,
                workspace,
                None,
            )
            .expect("session");
        let agent = tempfile::tempdir().expect("agent dir");
        std::fs::write(
            agent.path().join("settings.json"),
            r#"{"sandbox":{"mode":"danger-full-access"}}"#,
        )
        .expect("sandbox settings");
        let session_dir = tempfile::tempdir().expect("session dir");
        session.set_session_dir(session_dir.path().to_path_buf());
        if recorded {
            session.start_new_recording().expect("recording");
        }
        let mut options = pi_coding::ResourceManagerOptions::new(cwd);
        options.agent_dir = agent.path().to_path_buf();
        options.project_trust_override = Some(true);
        options.disable_extensions = true;
        options.disable_skills = true;
        options.disable_prompt_templates = true;
        options.disable_themes = true;
        options.disable_context_files = true;
        let resources = pi_coding::ResourceManager::new(options).expect("resources");
        session.attach_resources(resources).await.expect("attach resources");
        let application = pi_coding::Application::from_runtime_candidate(
            pi_coding::ApplicationRuntimeCandidate::new(session),
        )
        .await
        .expect("publish application");
        TestApplication { application, _agent: agent, _session_dir: session_dir }
    }

    fn range(start_line: usize, end_line: usize) -> FileConversationLineRange {
        FileConversationLineRange { start_line, end_line }
    }

    #[tokio::test]
    async fn owner_capture_unrecorded_has_no_identity() {
        let cwd = tempfile::tempdir().expect("cwd");
        std::fs::write(cwd.path().join("f.txt"), "x\n").expect("file");
        let fixture = test_application(cwd.path(), false).await;
        let owner = FileConversationOwner::capture(&fixture.application).expect("capture");
        assert_eq!(owner.recording, None);
        assert_eq!(owner.runtime_epoch, fixture.application.runtime_epoch());
        assert_eq!(owner.cwd, fixture.application.session().cwd().to_path_buf());
    }

    #[tokio::test]
    async fn owner_capture_recorded_binds_actual_identity() {
        let cwd_a = tempfile::tempdir().expect("cwd a");
        let fixture_a = test_application(cwd_a.path(), true).await;
        let cwd_b = tempfile::tempdir().expect("cwd b");
        let fixture_b = test_application(cwd_b.path(), true).await;
        let owner_a = FileConversationOwner::capture(&fixture_a.application).expect("capture a");
        let owner_b = FileConversationOwner::capture(&fixture_b.application).expect("capture b");

        let (writer_id, writer_path, writer_identity) =
            fixture_a.application.session().recording_writer_info().expect("writer info");
        let recording = owner_a.recording.as_ref().expect("recorded identity present");
        assert_eq!(recording.recorder_id, writer_id);
        assert_eq!(recording.session_path, writer_path);
        assert_eq!(recording.file_identity, writer_identity);
        assert_ne!(owner_a, owner_b);
        let replaced =
            FileConversationOwner { recording: owner_b.recording.clone(), ..owner_a.clone() };
        assert_ne!(owner_a, replaced);
    }

    #[tokio::test]
    async fn open_is_strict_and_binds_resolved_path() {
        let cwd = tempfile::tempdir().expect("cwd");
        std::fs::create_dir_all(cwd.path().join("nested/deep")).expect("dirs");
        std::fs::write(cwd.path().join("nested/deep/unique.txt"), "deep\n").expect("nested");
        let fixture = test_application(cwd.path(), false).await;
        let mut engine = FileConversationEngine::new();

        match engine.open(&fixture.application, "c1", "unique.txt").await {
            Err(FileConversationError::Read {
                code: Some(FileExplorerError::FileUnavailable),
                ..
            }) => {}
            other => panic!("expected strict unavailable, got {other:?}"),
        }
        assert!(engine.conversations.is_empty());

        let opened = engine
            .open(&fixture.application, "c1", "nested/deep/unique.txt")
            .await
            .expect("open");
        assert_eq!(opened.path, "nested/deep/unique.txt");
        assert_eq!(opened.conversation_id, "c1");
        assert!(opened.context.is_none());
        assert!(opened.entries.is_empty());

        std::fs::write(cwd.path().join("notes.txt"), "root notes\n").expect("root file");
        assert_eq!(
            engine.open(&fixture.application, "c1", "notes.txt").await.unwrap_err(),
            FileConversationError::BindingMismatch
        );
        assert_eq!(
            engine.snapshot(&fixture.application, "missing").await.unwrap_err(),
            FileConversationError::NotFound
        );
        engine.shutdown().await;
    }

    #[tokio::test]
    async fn prompt_rejections_are_typed_and_never_touch_the_controller() {
        let cwd = tempfile::tempdir().expect("cwd");
        std::fs::write(cwd.path().join("ctx.txt"), "l1\nl2\nl3\nl4\nl5\n").expect("file");
        let fixture = test_application(cwd.path(), false).await;
        let mut engine = FileConversationEngine::new();
        engine.open(&fixture.application, "c1", "ctx.txt").await.expect("open");

        assert_eq!(
            engine.prompt(&fixture.application, "c1", "m", Some(range(0, 3))).await.unwrap_err(),
            FileConversationError::SelectionInvalid { start_line: 0, end_line: 3 }
        );
        assert_eq!(
            engine.prompt(&fixture.application, "c1", "m", Some(range(5, 2))).await.unwrap_err(),
            FileConversationError::SelectionInvalid { start_line: 5, end_line: 2 }
        );
        assert_eq!(
            engine.prompt(&fixture.application, "c1", "m", Some(range(1, 2001))).await.unwrap_err(),
            FileConversationError::SelectionTooLarge { requested_lines: 2001, limit: 2000 }
        );
        assert_eq!(
            engine.prompt(&fixture.application, "c1", "m", Some(range(4, 9))).await.unwrap_err(),
            FileConversationError::SelectionOutOfRange {
                start_line: 4,
                end_line: 9,
                total_lines: 5
            }
        );
        assert_eq!(
            engine.prompt(&fixture.application, "c1", "m", Some(range(6, 8))).await.unwrap_err(),
            FileConversationError::SelectionOutOfRange {
                start_line: 6,
                end_line: 8,
                total_lines: 5
            }
        );
        match engine.prompt(&fixture.application, "c1", "m", Some(range(7, 8))).await {
            Err(FileConversationError::Read { code: None, message }) => {
                assert!(message.contains("cursor out of range"), "{message}");
            }
            other => panic!("expected preserved reader error, got {other:?}"),
        }
        assert_eq!(
            engine.prompt(&fixture.application, "c1", "   ", None).await.unwrap_err(),
            FileConversationError::PromptEmpty
        );
        assert_eq!(
            engine.prompt(&fixture.application, "c1", "a".repeat(8193).as_str(), None)
                .await
                .unwrap_err(),
            FileConversationError::PromptTooLarge { len_bytes: 8193, limit: 8192 }
        );
        engine.shutdown().await;
    }

    #[tokio::test]
    async fn prompt_selection_is_exact_and_initial_page_is_explicit() {
        let cwd = tempfile::tempdir().expect("cwd");
        std::fs::write(cwd.path().join("ctx.txt"), "l1\nl2\nl3\nl4\nl5\n").expect("file");
        let fixture = test_application(cwd.path(), false).await;
        let mut engine = FileConversationEngine::new();
        engine.open(&fixture.application, "c-sel", "ctx.txt").await.expect("open");

        let ack = engine
            .prompt(&fixture.application, "c-sel", "explain", Some(range(2, 4)))
            .await
            .expect("accepted prompt");
        assert!(ack.accepted && !ack.busy);
        let context = ack.snapshot.context.expect("selection context");
        assert_eq!(context.requested_range, Some(range(2, 4)));
        assert_eq!(context.included_range, Some(range(2, 4)));
        assert!(!context.truncated);
        let user = ack.snapshot.entries.last().expect("user entry");
        let text = user["text"].as_str().expect("user text");
        assert_eq!(text, "explain");
        let conversation = engine.conversations.get_mut("c-sel").expect("selection conversation");
        conversation.controller.poll_events();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            conversation.controller.wait_for_prompt_idle(),
        )
        .await
        .expect("selection prompt task timed out")
        .expect("selection prompt task");
        let captured = conversation.controller.agent_messages().await;
        let model = captured
            .iter()
            .rev()
            .find_map(|message| match message {
                pi_ai::Message::User(user) => Some(user),
                _ => None,
            })
            .expect("captured user message");
        let model_text = model
            .content
            .iter()
            .filter_map(|block| match block {
                pi_ai::ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();
        assert_ne!(model_text, text);
        assert!(model_text.contains("l2\nl3\nl4"), "{model_text}");
        assert!(model_text.contains("ctx.txt"), "{model_text}");
        assert!(model_text.contains("lines 2-4"), "{model_text}");
        assert!(model_text.starts_with("explain"), "{model_text}");

        engine.open(&fixture.application, "c-all", "ctx.txt").await.expect("open");
        let ack = engine
            .prompt(&fixture.application, "c-all", "summarize", None)
            .await
            .expect("accepted prompt");
        let context = ack.snapshot.context.expect("initial page context");
        assert_eq!(context.requested_range, None);
        assert_eq!(context.included_range, Some(range(1, 5)));
        assert!(!context.truncated);
        let user = ack.snapshot.entries.last().expect("user entry");
        let text = user["text"].as_str().expect("user text");
        assert_eq!(text, "summarize");
        let conversation = engine.conversations.get_mut("c-all").expect("page conversation");
        conversation.controller.poll_events();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            conversation.controller.wait_for_prompt_idle(),
        )
        .await
        .expect("page prompt task timed out")
        .expect("page prompt task");
        let captured = conversation.controller.agent_messages().await;
        let model_text = captured
            .iter()
            .rev()
            .find_map(|message| match message {
                pi_ai::Message::User(user) => Some(
                    user.content
                        .iter()
                        .filter_map(|block| match block {
                            pi_ai::ContentBlock::Text { text, .. } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<String>(),
                ),
                _ => None,
            })
            .expect("captured page message");
        assert_ne!(model_text, text);
        assert!(model_text.contains("l1\nl2\nl3\nl4\nl5"), "{model_text}");
        assert!(model_text.contains("lines 1-5"), "{model_text}");
        assert!(model_text.starts_with("summarize"), "{model_text}");
        engine.shutdown().await;
    }

    #[tokio::test]
    async fn capacity_is_eight_total_with_no_eviction() {
        let cwd = tempfile::tempdir().expect("cwd");
        for index in 1..=9 {
            std::fs::write(cwd.path().join(format!("f{index}.txt")), "x\n").expect("file");
        }
        let fixture = test_application(cwd.path(), false).await;
        let mut engine = FileConversationEngine::new();
        for index in 1..=8 {
            engine
                .open(&fixture.application, &format!("c{index}"), &format!("f{index}.txt"))
                .await
                .expect("open");
        }
        assert_eq!(
            engine.open(&fixture.application, "c9", "f9.txt").await.unwrap_err(),
            FileConversationError::Capacity { limit: 8 }
        );
        let closed = engine.close(&fixture.application, "c4").await.expect("close");
        assert_eq!(closed, json!({ "conversationId": "c4", "closed": true }));
        assert_eq!(
            engine.close(&fixture.application, "c4").await.unwrap_err(),
            FileConversationError::NotFound
        );
        engine.open(&fixture.application, "c9", "f9.txt").await.expect("open after close");
        engine.shutdown().await;
    }

    #[tokio::test]
    async fn stale_owner_is_retired_cleaned_up_and_superseded() {
        let cwd = tempfile::tempdir().expect("cwd");
        std::fs::write(cwd.path().join("a.txt"), "a\n").expect("file");
        let fixture = test_application(cwd.path(), false).await;
        let mut engine = FileConversationEngine::new();
        engine.open(&fixture.application, "c1", "a.txt").await.expect("open");

        assert_eq!(engine.is_busy(&fixture.application).await.expect("busy"), false);

        engine.conversations.get_mut("c1").expect("present").owner.runtime_epoch += 1;
        let retired = engine.retire_superseded(&fixture.application).await.expect("retire");
        assert_eq!(retired, vec!["c1".to_owned()]);
        assert!(engine.conversations.is_empty());
        assert_eq!(
            engine.close(&fixture.application, "c1").await.unwrap_err(),
            FileConversationError::NotFound
        );

        engine.open(&fixture.application, "c2", "a.txt").await.expect("reopen");
        engine.conversations.get_mut("c2").expect("present").owner.runtime_epoch += 1;
        assert_eq!(
            engine.snapshot(&fixture.application, "c2").await.unwrap_err(),
            FileConversationError::Superseded
        );
        assert!(engine.conversations.is_empty());
        assert_eq!(engine.shutdown().await, 0);
    }

    #[test]
    fn failure_wire_shapes_are_frozen() {
        assert_eq!(
            FileConversationError::Capacity { limit: 8 }.failure_data("c"),
            json!({ "conversationId": "c", "code": "file_conversation_capacity", "limit": 8 })
        );
        assert_eq!(
            FileConversationError::Superseded.failure_data("c"),
            json!({ "conversationId": "c", "code": "file_conversation_superseded" })
        );
        let read = FileConversationError::Read {
            message: "boom".to_owned(),
            code: Some(FileExplorerError::FileUnavailable),
        };
        assert_eq!(
            read.failure_data("c"),
            json!({ "conversationId": "c", "code": "file_unavailable" })
        );
        assert_eq!(read.code(), "file_conversation_read_failed");
        assert_eq!(
            FileConversationError::SelectionOutOfRange {
                start_line: 4,
                end_line: 9,
                total_lines: 5
            }
            .failure_data("c"),
            json!({
                "conversationId": "c",
                "code": "file_conversation_selection_out_of_range",
                "startLine": 4,
                "endLine": 9,
                "totalLines": 5
            })
        );
    }

    #[test]
    fn context_and_ack_wire_shapes() {
        let none = FileConversationContext {
            requested_range: None,
            included_range: None,
            truncated: false,
        };
        assert_eq!(none.to_wire_json(), json!({ "includedRange": null, "truncated": false }));
        let some = FileConversationContext {
            requested_range: Some(range(2, 4)),
            included_range: Some(range(2, 4)),
            truncated: false,
        };
        assert_eq!(
            some.to_wire_json(),
            json!({
                "requestedRange": { "startLine": 2, "endLine": 4 },
                "includedRange": { "startLine": 2, "endLine": 4 },
                "truncated": false
            })
        );
        let snapshot = FileConversationSnapshot {
            conversation_id: "c".to_owned(),
            path: "p".to_owned(),
            streaming: false,
            status: String::new(),
            entries: Vec::new(),
            context: None,
        };
        let idle =
            FileConversationPromptAck { snapshot: snapshot.clone(), accepted: true, busy: false };
        let wire = idle.to_wire_json();
        assert_eq!(wire["accepted"], json!(true));
        assert!(wire.get("busy").is_none());
        let busy = FileConversationPromptAck { snapshot, accepted: false, busy: true };
        let wire = busy.to_wire_json();
        assert_eq!(wire["accepted"], json!(false));
        assert_eq!(wire["busy"], json!(true));
    }
}
