//! Detached side-chat fork helpers for `/btw`.
//!
//! Builds an independent in-memory agent context from the main application's
//! active leaf without replacing or mutating the live runtime/session.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use pi_agent::{
    AfterToolCallFn, Agent, AgentOptions, AgentState, AgentTool, BeforeToolCallFn, StreamFn,
    ThinkingLevel, ToolCapability,
};
use pi_ai::{Message, Model, Schema, SimpleStreamOptions};
use serde_json::Value;

use super::Application;

/// Snapshot used to construct a detached side-chat agent.
///
/// Cloning this value does not touch the main session file or transcript.
#[derive(Clone)]
pub struct SideChatFork {
    pub messages: Vec<Message>,
    pub system_prompt: String,
    pub model: Model,
    pub thinking_level: ThinkingLevel,
    pub cwd: PathBuf,
    pub stream_fn: StreamFn,
    pub stream_options: SimpleStreamOptions,
    pub api_key: String,
    pub auth_resolver: Option<crate::SessionAuthResolver>,
    pub leaf_id: Option<String>,
    pub session_id: Option<String>,
    pub session_file: Option<PathBuf>,
    pub permission_rules: Option<crate::PermissionRulesSource>,
    pub before_tool_call: Option<BeforeToolCallFn>,
    pub approve_tool_call: Option<BeforeToolCallFn>,
    pub after_tool_call: Option<AfterToolCallFn>,
}

/// Read-only view of main session history for the side-chat `peek_main` tool.
#[derive(Clone, Debug)]
pub struct SideChatMainPeek {
    pub session_id: Option<String>,
    pub session_file: Option<PathBuf>,
    pub leaf_id: Option<String>,
    pub messages: Vec<Message>,
}

const SIDE_CHAT_SYSTEM_SUFFIX: &str = "\n\n# Side chat\nYou are running in a side chat forked from the main conversation. \
Your messages stay in this side panel and are never merged into the main session. \
Default tools are read-only. The user may enable edit mode explicitly. \
Use peek_main to inspect the main conversation when needed.";

const CODE_REVIEW_SYSTEM_SUFFIX: &str = "\n\n# Code review\nReview and modify the owning workspace as requested. \
Use the same governed tools and permissions as Main.";

impl Application {
    /// Capture an independent fork of the active conversation for side chat.
    ///
    /// This never replaces the main runtime, never writes session files, and
    /// never mutates main history/messages.
    pub async fn fork_side_chat(&self) -> Result<SideChatFork> {
        Ok(self.fork_side_chat_with_active_tools().await?.0)
    }

    /// Capture fork + exact active tools from one Session handle so Code
    /// Review fork/refork never mix two Application lookups.
    pub async fn fork_side_chat_with_active_tools(&self) -> Result<(SideChatFork, Vec<AgentTool>)> {
        let session = self.session();
        let options = session.child_session_options_snapshot();
        let messages = session.history();
        let system_prompt = session.system_prompt().await;
        let recorder = session.recorder_info();
        let (session_id, session_file) = recorder
            .as_ref()
            .map(|(id, path)| (Some(id.clone()), Some(path.clone())))
            .unwrap_or((None, None));
        // Recorded sessions derive the active leaf from the persisted session
        // tree; a corrupt or truncated session file must fail the fork instead
        // of silently continuing without a leaf (which would fall back to
        // live-message counts). Unrecorded sessions have no tree to consult
        // and fork leaf-free.
        let leaf_id = match recorder {
            Some(_) => {
                let tree = session.session_tree()?;
                tree.active_leaf_id.clone().or_else(|| tree.leaf_id.clone())
            }
            None => None,
        };
        let tools = session.active_tools();
        Ok((
            SideChatFork {
                messages,
                system_prompt,
                model: options.model,
                thinking_level: options.thinking_level,
                cwd: options.workspace.cwd().to_path_buf(),
                stream_fn: options.stream_fn,
                stream_options: detached_stream_options(options.stream_options),
                api_key: options.api_key,
                auth_resolver: options.auth_resolver,
                leaf_id,
                session_id,
                session_file,
                permission_rules: session.permission_rules_source(),
                // Fork-time frozen governance: the detached agent must fire the
                // host hooks captured AT THE FORK, never live Main replacements.
                before_tool_call: session.detached_before_tool_call(),
                approve_tool_call: session.approve_tool_call(),
                after_tool_call: session.detached_after_tool_call(),
            },
            tools,
        ))
    }

    /// Build a standalone side-chat agent from a previously captured fork.
    #[must_use]
    pub fn create_side_chat_agent(fork: &SideChatFork, tools: Vec<AgentTool>) -> Agent {
        create_detached_agent(fork, tools, Some(SIDE_CHAT_SYSTEM_SUFFIX))
    }

    /// Build a standalone code-review agent with Main-equivalent tools.
    #[must_use]
    pub fn create_code_review_agent(fork: &SideChatFork, tools: Vec<AgentTool>) -> Agent {
        create_detached_agent(fork, tools, Some(CODE_REVIEW_SYSTEM_SUFFIX))
    }

    /// Read-only snapshot of the main session's active history. Recorded sessions
    /// are traversed root-to-active-leaf; unrecorded sessions use live memory.
    pub fn peek_main_history(&self, since: Option<&str>) -> Result<SideChatMainPeek> {
        let session = self.session();
        let Some((session_id, session_file)) = session.recorder_info() else {
            if let Some(since) = since {
                return Err(anyhow!(
                    "Entry id {since} is unavailable because session recording is disabled"
                ));
            }
            return Ok(SideChatMainPeek {
                session_id: None,
                session_file: None,
                leaf_id: None,
                messages: session.history(),
            });
        };

        let all_entries = session.session_entries(None)?;
        let tree = session.session_tree()?;
        let leaf_id = tree
            .active_leaf_id
            .clone()
            .or(tree.leaf_id.clone())
            .or(all_entries.leaf_id);
        let by_id = all_entries
            .entries
            .iter()
            .enumerate()
            .map(|(index, entry)| (entry.id.as_str(), index))
            .collect::<HashMap<_, _>>();
        let mut branch_indices = Vec::new();
        let mut visited = HashSet::new();
        let mut current = leaf_id.as_deref().and_then(|id| by_id.get(id).copied());
        while let Some(index) = current {
            let entry = &all_entries.entries[index];
            if !visited.insert(entry.id.as_str()) {
                break;
            }
            branch_indices.push(index);
            current = entry
                .parent_id
                .as_deref()
                .and_then(|parent| by_id.get(parent).copied());
        }
        branch_indices.reverse();
        let start = since.map_or(Ok(0), |since| {
            branch_indices
                .iter()
                .position(|index| all_entries.entries[*index].id == since)
                .map(|index| index + 1)
                .ok_or_else(|| anyhow!("Entry not found on active branch: {since}"))
        })?;
        let messages = branch_indices[start..]
            .iter()
            .filter_map(|index| all_entries.entries[*index].message.clone())
            .collect();
        Ok(SideChatMainPeek {
            session_id: Some(session_id),
            session_file: Some(session_file),
            leaf_id,
            messages,
        })
    }

    /// Read-only built-in tool set for a detached side-chat agent, bound to
    /// this application's live generation so PolicyFs-backed read tools
    /// (read/grep/find/glob/ls/ast_grep/web_search) execute real filesystem
    /// authority. The generation-less constructors fail closed.
    #[must_use]
    pub fn create_side_chat_read_only_tools(&self, cwd: &str) -> Vec<AgentTool> {
        crate::create_read_only_tools_for_workspace_with_generation(
            crate::WorkspaceRoots::for_tool_factory(cwd),
            self.generation_capture_fn(),
        )
    }

    /// Convenience: fork + create agent with the default read-only tool set.
    pub async fn open_side_chat_agent(&self) -> Result<(SideChatFork, Agent)> {
        let fork = self.fork_side_chat().await?;
        let cwd = fork.cwd.to_string_lossy().into_owned();
        let tools = self.create_side_chat_read_only_tools(&cwd);
        let agent = Self::create_side_chat_agent(&fork, tools);
        Ok((fork, agent))
    }
}
fn detached_stream_options(mut options: SimpleStreamOptions) -> SimpleStreamOptions {
    options.stream.session_id = Some(uuid::Uuid::now_v7().to_string());
    options.stream.on_payload = None;
    options.stream.on_response = None;
    options.stream.before_provider_request = None;
    options.stream.before_provider_headers = None;
    options.stream.after_provider_response = None;
    options.stream.abort_signal = None;
    options
}


fn detached_stream_fn(fork: &SideChatFork) -> StreamFn {
    let fallback = fork.stream_fn.clone();
    let Some(resolver) = fork.auth_resolver.clone() else {
        return fallback;
    };
    Arc::new(move |model, context, mut options| {
        let resolver = resolver.clone();
        let fallback = fallback.clone();
        let future: pi_agent::BoxFuture<pi_ai::AssistantMessageEventStream> =
            Box::pin(async move {
                match resolver(model.clone()).await {
                    Ok(auth) => {
                        options.stream.api_key = Some(auth.api_key);
                        merge_headers_case_insensitive(&mut options.stream.headers, auth.headers);
                        options.stream.env.extend(auth.env);
                        fallback(model, context, options).await
                    }
                    Err(error) => auth_error_stream(&model, error.to_string()).await,
                }
            });
        future
    })
}

fn create_detached_agent(
    fork: &SideChatFork,
    tools: Vec<AgentTool>,
    suffix: Option<&str>,
) -> Agent {
    let mut system_prompt = fork.system_prompt.clone();
    if let Some(suffix) = suffix {
        system_prompt.push_str(suffix);
    }
    let get_api_key = if fork.auth_resolver.is_some() {
        None
    } else {
        let api_key = fork.api_key.clone();
        if api_key.is_empty() {
            None
        } else {
            Some(Arc::new(move |_provider: &str| Some(api_key.clone())) as pi_agent::GetApiKeyFn)
        }
    };
    Agent::new(AgentOptions {
        initial_state: AgentState {
            system_prompt,
            model: fork.model.clone(),
            thinking_level: fork.thinking_level,
            tools,
            messages: fork.messages.clone(),
            ..AgentState::default()
        },
        stream_fn: detached_stream_fn(fork),
        get_api_key,
        stream_options: fork.stream_options.clone(),
        before_tool_call: fork.before_tool_call.clone(),
        approve_tool_call: fork.approve_tool_call.clone(),
        after_tool_call: fork.after_tool_call.clone(),
        ..AgentOptions::default()
    })
}

fn merge_headers_case_insensitive(
    headers: &mut std::collections::HashMap<String, String>,
    source: std::collections::HashMap<String, String>,
) {
    for (name, value) in source {
        if let Some(existing) = headers
            .keys()
            .find(|existing| existing.eq_ignore_ascii_case(&name))
            .cloned()
        {
            headers.remove(&existing);
        }
        headers.insert(name, value);
    }
}

async fn auth_error_stream(model: &Model, message: String) -> pi_ai::AssistantMessageEventStream {
    let stream = pi_ai::new_assistant_message_event_stream();
    let mut error = pi_ai::AssistantMessage::pending(model);
    error.stop_reason = pi_ai::StopReason::Error;
    error.error_message = Some(message);
    stream
        .push(pi_ai::AssistantMessageEvent::Error {
            reason: pi_ai::StopReason::Error,
            error: error.clone(),
        })
        .await;
    stream.end(Some(error)).await;
    stream
}

/// Filter tools by explicit [`ToolCapability`] metadata (never by tool name).
#[must_use]
pub fn filter_tools_by_capabilities(
    tools: impl IntoIterator<Item = AgentTool>,
    allow: &[ToolCapability],
) -> Vec<AgentTool> {
    tools
        .into_iter()
        .filter(|tool| allow.contains(&tool.capability))
        .collect()
}

/// True when the tool set contains any Write or Exec capability.
#[must_use]
pub fn tools_include_mutation(tools: &[AgentTool]) -> bool {
    tools.iter().any(|tool| {
        matches!(
            tool.capability,
            ToolCapability::Write | ToolCapability::Exec
        )
    })
}

#[must_use]
pub(crate) fn is_session_orchestration_tool(name: &str) -> bool {
    matches!(
        name,
        "task"
            | "hub"
            | "todo_child_send"
            | "todo_child_add_task"
            | "todo_child_list"
            | "todo_child_status"
            | "todo_child_select"
            | "todo_child_stop"
            | "todo_child_pause"
            | "todo_child_resume"
            | "todo_child_cancel"
            | "todo_child_archive"
            | "todo_child_remove"
    )
}

#[must_use]
pub fn bind_code_review_orchestration_tools(
    tools: Vec<AgentTool>,
    orchestration: Option<&crate::OrchestrationRuntime>,
) -> Vec<AgentTool> {
    let mut tools = tools
        .into_iter()
        .filter(|tool| !is_session_orchestration_tool(&tool.name))
        .collect::<Vec<_>>();
    if let Some(orchestration) = orchestration {
        tools.extend(orchestration.agent_tools(crate::CODE_REVIEW_PARENT_AGENT_ID, 0));
    }
    tools
}

/// Build the side-chat `peek_main` tool bound to a main-history snapshot provider.
///
/// The provider receives `(since_fork, since_entry_id)`.
#[must_use]
pub fn create_peek_main_tool<F>(provider: F) -> AgentTool
where
    F: Fn(bool, Option<String>) -> Result<SideChatMainPeek> + Send + Sync + 'static,
{
    let parameters = Schema::object_ordered(vec![
        (
            "since_fork".to_owned(),
            Schema {
                schema_type: Some(Value::String("boolean".into())),
                description: Some(
                    "When true, return only main messages after the side-chat fork point."
                        .to_owned(),
                ),
                ..Schema::default()
            },
            false,
        ),
        (
            "since".to_owned(),
            Schema {
                schema_type: Some(Value::String("string".into())),
                description: Some(
                    "Optional entry id; return main messages after this entry.".to_owned(),
                ),
                ..Schema::default()
            },
            false,
        ),
    ]);
    AgentTool::new(
        "peek_main",
        "Read-only view of the main conversation history. Does not modify the main session.",
        parameters,
        move |context| {
            let since_fork = context
                .arguments
                .get("since_fork")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let since = context
                .arguments
                .get("since")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .filter(|value| !value.is_empty());
            let peek = provider(since_fork, since).map_err(|error| anyhow!("{error:#}"));
            async move {
                let peek = peek?;
                let body = serde_json::json!({
                    "sessionId": peek.session_id,
                    "sessionFile": peek.session_file.as_ref().map(|path| path.display().to_string()),
                    "leafId": peek.leaf_id,
                    "messageCount": peek.messages.len(),
                    "messages": peek.messages,
                });
                Ok(pi_agent::AgentToolResult {
                    content: vec![pi_ai::ContentBlock::text(serde_json::to_string_pretty(&body)?)],
                    details: body,
                    ..pi_agent::AgentToolResult::default()
                })
            }
        },
    )
    .with_capability(ToolCapability::Read)
    .with_label("Peek main")
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use pi_agent::StreamFn;
    use pi_ai::{
        AssistantMessage, Model, StopReason, Transport, new_assistant_message_event_stream,
        providers::{FauxProviderOptions, FauxProviderRegistration, register_faux_provider},
    };

    struct SideChatTestEnvironment {
        cwd: tempfile::TempDir,
        agent_dir: tempfile::TempDir,
        session_dir: tempfile::TempDir,
        _faux: FauxProviderRegistration,
        model: Model,
    }

    impl SideChatTestEnvironment {
        fn new() -> Self {
            let cwd = tempfile::tempdir().expect("cwd");
            let agent_dir = tempfile::tempdir().expect("agent dir");
            let session_dir = tempfile::tempdir().expect("session dir");
            std::fs::write(
                agent_dir.path().join("settings.json"),
                r#"{"sandbox":{"mode":"danger-full-access"}}"#,
            )
            .expect("isolated settings");
            let suffix = uuid::Uuid::now_v7().simple().to_string();
            let mut model = Model::default();
            model.id = format!("side-chat-{suffix}");
            model.name = "Side chat fixture".to_owned();
            model.api = format!("side-chat-api-{suffix}");
            model.provider = format!("side-chat-provider-{suffix}");
            model.base_url = "http://localhost:0".to_owned();
            let faux = register_faux_provider(FauxProviderOptions {
                api: model.api.clone(),
                provider: model.provider.clone(),
                models: vec![model.clone()],
                chunk_size: 12,
            });
            Self {
                cwd,
                agent_dir,
                session_dir,
                _faux: faux,
                model,
            }
        }

        fn session(
            &self,
            api_key: &str,
            stream_options: SimpleStreamOptions,
            stream_fn: Option<StreamFn>,
            auth_resolver: Option<crate::SessionAuthResolver>,
        ) -> crate::Session {
            let workspace = crate::WorkspaceRoots::new(self.cwd.path(), Vec::<PathBuf>::new())
                .expect("workspace");
            let session =
                crate::Session::new_with_additional_tools_filtered_discovery_workspace_and_uri(
                    crate::SessionOptions {
                        model: self.model.clone(),
                        cwd: self.cwd.path().to_path_buf(),
                        system_prompt: "main system".to_owned(),
                        thinking_level: ThinkingLevel::Off,
                        api_key: api_key.to_owned(),
                        compaction: None,
                        stream_options,
                        tools: Some(Vec::new()),
                        before_tool_call: None,
                        after_tool_call: None,
                        stream_fn,
                        auth_resolver,
                        auth_manager: None,
                    },
                    Vec::new(),
                    crate::ToolSelection::default(),
                    crate::ResourceDiscovery::Disabled,
                    workspace,
                    None,
                )
                .expect("session");
            session.set_session_dir(self.session_dir.path().to_path_buf());
            session
        }

        async fn retain_recording(&self, session: &crate::Session, messages: &[Message]) {
            let recorder = crate::start_session_in(
                self.cwd.path(),
                Some(&self.model),
                Some("off"),
                Some(self.session_dir.path()),
                None,
                None,
            )
            .expect("recorder");
            for message in messages {
                recorder.record_message(message).expect("record message");
            }
            recorder.persist_now().expect("persist retained recording");
            session.record(recorder).expect("attach recorder");
            session
                .load_history(messages.to_vec())
                .await
                .expect("history");
        }

        async fn publish(&self, session: &crate::Session) -> Application {
            let mut options = crate::ResourceManagerOptions::new(self.cwd.path());
            options.agent_dir = self.agent_dir.path().to_path_buf();
            options.project_trust_override = Some(true);
            options.disable_extensions = true;
            options.disable_skills = true;
            options.disable_prompt_templates = true;
            options.disable_themes = true;
            options.disable_context_files = true;
            let resources = crate::ResourceManager::new(options).expect("resources");
            session
                .attach_resources(resources)
                .await
                .expect("attach resources");
            Application::from_runtime_candidate(crate::ApplicationRuntimeCandidate::new(
                session.clone(),
            ))
            .await
            .expect("publish application")
        }
    }

    #[tokio::test]
    async fn fork_does_not_mutate_main_session_identity_or_messages() {
        let environment = SideChatTestEnvironment::new();
        let session = environment.session("test-key", Default::default(), None, None);
        environment
            .retain_recording(&session, &[Message::user_text("hello from main", 1)])
            .await;
        let application = environment.publish(&session).await;
        let before = application.state().await;
        let before_messages = application.messages();
        let before_id = before.session_id.clone();
        let before_file = before.session_file.clone();

        let (fork, agent) = application
            .open_side_chat_agent()
            .await
            .expect("side agent");
        assert_eq!(fork.messages.len(), before_messages.len());
        assert!(
            !tools_include_mutation(&agent.state().await.tools),
            "default side tools must be read-only"
        );

        // Prompting the side agent must not touch main state. Use a no-op stream
        // is not required: merely constructing and inspecting is enough here.
        let after = application.state().await;
        assert_eq!(after.session_id, before_id);
        assert_eq!(after.session_file, before_file);
        assert_eq!(application.messages(), before_messages);
        assert_eq!(after.message_count, before.message_count);
    }

    #[tokio::test]
    async fn fork_propagates_corrupt_recorded_tree_but_succeeds_unrecorded() {
        let environment = SideChatTestEnvironment::new();

        // Unrecorded sessions have no session tree to consult and must still
        // fork: leaf-free and without recorder identity.
        let plain = environment.session("test-key", Default::default(), None, None);
        let application = environment.publish(&plain).await;
        let (fork, _agent) = application
            .open_side_chat_agent()
            .await
            .expect("unrecorded fork succeeds");
        assert!(fork.leaf_id.is_none());
        assert!(fork.session_id.is_none());
        assert!(fork.session_file.is_none());

        // Recorded sessions derive the leaf from the persisted tree, so a
        // corrupt/truncated session file must surface as a fork error instead
        // of being swallowed into a leaf-less fallback fork.
        let recorded = environment.session("test-key", Default::default(), None, None);
        let recorder = crate::start_session_in(
            environment.cwd.path(),
            Some(&environment.model),
            Some("off"),
            Some(environment.session_dir.path()),
            None,
            None,
        )
        .expect("recorder");
        let recorded_id = recorder.id();
        let recorded_path = recorder.path();
        let recorded_leaf = recorder
            .record_message(&Message::user_text("recorded", 1))
            .expect("record message");
        recorder.persist_now().expect("persist retained recording");
        let original_file = std::fs::read(&recorded_path).expect("read recorded session file");
        recorded.record(recorder).expect("attach recorder");
        recorded
            .load_history(vec![Message::user_text("recorded", 1)])
            .await
            .expect("history");
        std::fs::write(&recorded_path, b"{").expect("truncate recorded session file");
        let application = environment.publish(&recorded).await;
        let error = match application.open_side_chat_agent().await {
            Ok(_) => panic!("corrupt recorded session must fail the fork"),
            Err(error) => error,
        };
        assert!(
            format!("{error:#}").contains("parsing session"),
            "unexpected error: {error:#}"
        );

        // A valid recorded session still forks with the exact active leaf and
        // keeps recorder identity — no message-count fallback.
        std::fs::write(&recorded_path, &original_file).expect("restore recorded session file");
        let (fork, _agent) = application
            .open_side_chat_agent()
            .await
            .expect("restored recorded fork succeeds");
        assert_eq!(fork.leaf_id.as_deref(), Some(recorded_leaf.as_str()));
        assert_eq!(fork.session_id.as_deref(), Some(recorded_id.as_str()));
        assert_eq!(fork.session_file.as_deref(), Some(recorded_path.as_path()));
    }

    #[tokio::test]
    async fn peek_main_is_read_only_and_returns_main_messages() {
        let environment = SideChatTestEnvironment::new();
        let session = environment.session("test-key", Default::default(), None, None);
        environment
            .retain_recording(
                &session,
                &[Message::user_text("one", 1), Message::user_text("two", 2)],
            )
            .await;
        let application = environment.publish(&session).await;
        let peek = application.peek_main_history(None).expect("peek");
        assert_eq!(peek.messages.len(), 2);
        let before = application.messages();
        let _ = application.peek_main_history(None).expect("peek again");
        assert_eq!(application.messages(), before);
    }

    #[tokio::test]
    async fn fork_scrubs_provider_hooks_and_refreshes_auth_with_independent_session_id() {
        let environment = SideChatTestEnvironment::new();
        let hook_calls = Arc::new(AtomicUsize::new(0));
        let resolver_calls = Arc::new(AtomicUsize::new(0));
        let captured = Arc::new(std::sync::Mutex::new(None));
        let mut stream_options = SimpleStreamOptions::default();
        stream_options.stream.transport = Transport::Sse;
        stream_options.stream.headers = HashMap::from([
            ("X-Static".to_owned(), "static".to_owned()),
            ("X-Refresh".to_owned(), "stale".to_owned()),
        ]);
        stream_options
            .stream
            .env
            .insert("STATIC_ENV".to_owned(), "static".to_owned());
        stream_options.stream.timeout_ms = Some(12_345);
        let payload_calls = hook_calls.clone();
        stream_options.stream.on_payload = Some(Arc::new(move |payload, _| {
            payload_calls.fetch_add(1, Ordering::SeqCst);
            Ok(payload)
        }));
        let response_calls = hook_calls.clone();
        stream_options.stream.on_response = Some(Arc::new(move |_, _| {
            response_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }));
        let request_calls = hook_calls.clone();
        stream_options.stream.before_provider_request = Some(Arc::new(move |payload, _| {
            request_calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move { Ok(payload) })
        }));
        let header_calls = hook_calls.clone();
        stream_options.stream.before_provider_headers = Some(Arc::new(move |headers, _| {
            header_calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move { Ok(headers) })
        }));
        let after_calls = hook_calls.clone();
        stream_options.stream.after_provider_response = Some(Arc::new(move |_, _| {
            after_calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move { Ok(()) })
        }));

        let captured_options = captured.clone();
        let stream_fn: StreamFn = Arc::new(move |model, _context, options| {
            *captured_options.lock().expect("capture options") = Some(options);
            Box::pin(async move {
                let stream = new_assistant_message_event_stream();
                let mut message = AssistantMessage::pending(&model);
                message.stop_reason = StopReason::Stop;
                stream.end(Some(message)).await;
                stream
            })
        });
        let refresh_calls = resolver_calls.clone();
        let resolver: crate::SessionAuthResolver = Arc::new(move |_model| {
            refresh_calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                Ok(crate::RequestAuth {
                    api_key: "refreshed-key".to_owned(),
                    headers: HashMap::from([
                        ("x-refresh".to_owned(), "fresh".to_owned()),
                        ("X-Auth".to_owned(), "auth".to_owned()),
                    ]),
                    env: HashMap::from([("REFRESHED_ENV".to_owned(), "fresh".to_owned())]),
                    available_model_ids: None,
                    kind: crate::RequestAuthKind::ApiKeyOrHeaders,
                })
            })
        });
        std::fs::write(
            environment.agent_dir.path().join("settings.json"),
            r#"{"sandbox":{"mode":"danger-full-access"},"transport":"sse","timeoutMs":12345}"#,
        )
        .expect("auth stream settings");
        let session =
            environment.session("stale-key", stream_options, Some(stream_fn), Some(resolver));
        environment.retain_recording(&session, &[]).await;
        let main_options = session.stream_options();
        let main_session_id = main_options
            .stream
            .session_id
            .clone()
            .expect("main session id");
        let application = environment.publish(&session).await;
        resolver_calls.store(0, Ordering::SeqCst);
        let (fork, agent) = application
            .open_side_chat_agent()
            .await
            .expect("side agent");

        assert_ne!(
            fork.stream_options.stream.session_id.as_deref(),
            Some(main_session_id.as_str())
        );
        assert_eq!(fork.stream_options.stream.transport, Transport::Sse);
        assert_eq!(fork.stream_options.stream.timeout_ms, Some(12_345));
        assert_eq!(
            fork.stream_options
                .stream
                .headers
                .get("X-Static")
                .map(String::as_str),
            Some("static")
        );
        assert_eq!(
            fork.stream_options
                .stream
                .env
                .get("STATIC_ENV")
                .map(String::as_str),
            Some("static")
        );
        assert!(fork.stream_options.stream.on_payload.is_none());
        assert!(fork.stream_options.stream.on_response.is_none());
        assert!(fork.stream_options.stream.before_provider_request.is_none());
        assert!(fork.stream_options.stream.before_provider_headers.is_none());
        assert!(fork.stream_options.stream.after_provider_response.is_none());

        agent.prompt("side request").await.expect("side prompt");
        let observed = captured
            .lock()
            .expect("captured options")
            .clone()
            .expect("stream options");
        assert_eq!(resolver_calls.load(Ordering::SeqCst), 1);
        assert_eq!(hook_calls.load(Ordering::SeqCst), 0);
        assert_eq!(observed.stream.api_key.as_deref(), Some("refreshed-key"));
        assert_ne!(observed.stream.api_key.as_deref(), Some("stale-key"));
        assert_eq!(
            observed.stream.headers.get("X-Static").map(String::as_str),
            Some("static")
        );
        assert_eq!(
            observed.stream.headers.get("x-refresh").map(String::as_str),
            Some("fresh")
        );
        assert!(!observed.stream.headers.contains_key("X-Refresh"));
        assert_eq!(
            observed.stream.headers.get("X-Auth").map(String::as_str),
            Some("auth")
        );
        assert_eq!(
            observed.stream.env.get("STATIC_ENV").map(String::as_str),
            Some("static")
        );
        assert_eq!(
            observed.stream.env.get("REFRESHED_ENV").map(String::as_str),
            Some("fresh")
        );
        assert_eq!(
            observed.stream.session_id,
            fork.stream_options.stream.session_id
        );
        assert!(observed.stream.on_payload.is_none());
        assert!(observed.stream.on_response.is_none());
        assert!(observed.stream.before_provider_request.is_none());
        assert!(observed.stream.before_provider_headers.is_none());
        assert!(observed.stream.after_provider_response.is_none());
    }

    #[tokio::test]
    async fn peek_main_uses_active_recorded_branch_and_entry_id_slicing() {
        let environment = SideChatTestEnvironment::new();
        let session = environment.session("test-key", Default::default(), None, None);
        let recorder = crate::start_session_in(
            environment.cwd.path(),
            Some(&environment.model),
            Some("off"),
            Some(environment.session_dir.path()),
            Some("side-peek-branch"),
            None,
        )
        .expect("recorder");
        let root = recorder
            .record_message(&Message::user_text("root", 1))
            .expect("root");
        let abandoned = recorder
            .record_message(&Message::user_text("abandoned", 2))
            .expect("abandoned");
        recorder.fork_from(Some(&root));
        let active = recorder
            .record_message(&Message::user_text("active", 3))
            .expect("active");
        recorder.persist_now().expect("persist retained branch");
        session.record(recorder).expect("attach recorder");
        session
            .load_history(vec![
                Message::user_text("root", 1),
                Message::user_text("active", 3),
            ])
            .await
            .expect("active history");
        let application = environment.publish(&session).await;

        let full = application
            .peek_main_history(None)
            .expect("full active branch");
        assert_eq!(full.leaf_id.as_deref(), Some(active.as_str()));
        assert_eq!(
            full.messages,
            vec![
                Message::user_text("root", 1),
                Message::user_text("active", 3)
            ]
        );
        let since_root = application
            .peek_main_history(Some(&root))
            .expect("since root");
        assert_eq!(since_root.messages, vec![Message::user_text("active", 3)]);
        assert!(application.peek_main_history(Some(&abandoned)).is_err());
    }

    #[test]
    fn capability_filter_drops_write_and_exec() {
        let cwd = tempfile::tempdir().expect("cwd");
        let tools = crate::create_coding_tools(&cwd.path().to_string_lossy());
        assert!(tools_include_mutation(&tools));
        let read_only = filter_tools_by_capabilities(tools, &[ToolCapability::Read]);
        assert!(!tools_include_mutation(&read_only));
        assert!(
            read_only
                .iter()
                .all(|tool| tool.capability == ToolCapability::Read)
        );
    }
}
