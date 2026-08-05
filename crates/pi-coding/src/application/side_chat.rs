//! Detached side-chat fork helpers for `/btw`.
//!
//! Builds an independent in-memory agent context from the main application's
//! active leaf without replacing or mutating the live runtime/session.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use pi_agent::{Agent, AgentOptions, AgentState, AgentTool, StreamFn, ThinkingLevel, ToolCapability};
use pi_ai::{Message, Model, SimpleStreamOptions, Schema};
use serde_json::Value;

use super::Application;

/// Snapshot used to construct a detached side-chat agent.
///
/// Cloning this value does not touch the main session file or transcript.
#[derive(Clone)]
pub struct SideChatFork {
    /// Full branch messages at the active leaf (deep-copied).
    pub messages: Vec<Message>,
    pub system_prompt: String,
    pub model: Model,
    pub thinking_level: ThinkingLevel,
    pub cwd: PathBuf,
    pub stream_fn: StreamFn,
    pub stream_options: SimpleStreamOptions,
    pub api_key: String,
    /// Request-time credential resolver retained without binding the main Application.
    pub auth_resolver: Option<crate::SessionAuthResolver>,
    /// Active leaf id at fork time (for `peek_main` since-fork slicing).
    pub leaf_id: Option<String>,
    /// Main recorder session id (informational only; side chat never writes here).
    pub session_id: Option<String>,
    /// Main session file path (informational only).
    pub session_file: Option<PathBuf>,
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

impl Application {
    /// Capture an independent fork of the active conversation for side chat.
    ///
    /// This never replaces the main runtime, never writes session files, and
    /// never mutates main history/messages.
    pub async fn fork_side_chat(&self) -> Result<SideChatFork> {
        let session = self.session();
        let options = session.child_session_options_snapshot();
        let messages = session.history();
        let system_prompt = session.system_prompt().await;
        let tree = session.session_tree().ok();
        let leaf_id = tree
            .as_ref()
            .and_then(|tree| tree.active_leaf_id.clone().or_else(|| tree.leaf_id.clone()));
        let (session_id, session_file) = session
            .recorder_info()
            .map(|(id, path)| (Some(id), Some(path)))
            .unwrap_or((None, None));
        Ok(SideChatFork {
            messages,
            system_prompt,
            model: options.model,
            thinking_level: options.thinking_level,
            cwd: options.cwd,
            stream_fn: options.stream_fn,
            stream_options: detached_stream_options(options.stream_options),
            api_key: options.api_key,
            auth_resolver: options.auth_resolver,
            leaf_id,
            session_id,
            session_file,
        })
    }

    /// Build a standalone [`Agent`] from a previously captured fork.
    ///
    /// The agent runs its own loop and event stream. It does not share
    /// recorders, process ownership, or runtime slots with the main application.
    #[must_use]
    pub fn create_side_chat_agent(fork: &SideChatFork, tools: Vec<AgentTool>) -> Agent {
        let mut system_prompt = fork.system_prompt.clone();
        if !system_prompt.contains("# Side chat") {
            system_prompt.push_str(SIDE_CHAT_SYSTEM_SUFFIX);
        }
        let get_api_key = if fork.auth_resolver.is_some() {
            None
        } else {
            let api_key = fork.api_key.clone();
            if api_key.is_empty() {
                None
            } else {
                Some(Arc::new(move |_provider: &str| Some(api_key.clone()))
                    as pi_agent::GetApiKeyFn)
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
            ..AgentOptions::default()
        })
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
            .or(tree.leaf_id)
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

    /// Convenience: fork + create agent with the default read-only tool set.
    pub async fn open_side_chat_agent(&self) -> Result<(SideChatFork, Agent)> {
        let fork = self.fork_side_chat().await?;
        let cwd = fork.cwd.to_string_lossy().into_owned();
        let tools = crate::create_read_only_tools(&cwd);
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
        let future: pi_agent::BoxFuture<pi_ai::AssistantMessageEventStream> = Box::pin(async move {
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

async fn auth_error_stream(
    model: &Model,
    message: String,
) -> pi_ai::AssistantMessageEventStream {
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use pi_agent::StreamFn;
    use pi_ai::{AssistantMessage, Model, StopReason, Transport, new_assistant_message_event_stream};

    fn test_session(cwd: &std::path::Path) -> crate::Session {
        crate::Session::new(crate::SessionOptions {
            model: Model::default(),
            cwd: cwd.to_path_buf(),
            system_prompt: "main system".to_owned(),
            thinking_level: ThinkingLevel::Off,
            api_key: "test-key".to_owned(),
            compaction: None,
            stream_options: Default::default(),
            tools: Some(crate::create_coding_tools(&cwd.to_string_lossy())),
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: None,
            auth_resolver: None,
        })
        .expect("session")
    }

    #[tokio::test]
    async fn fork_does_not_mutate_main_session_identity_or_messages() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = test_session(cwd.path());
        session
            .load_history(vec![Message::user_text("hello from main", 1)])
            .await
            .expect("history");
        let application = Application::new(session).await;
        let before = application.state().await;
        let before_messages = application.messages();
        let before_id = before.session_id.clone();
        let before_file = before.session_file.clone();

        let (fork, agent) = application.open_side_chat_agent().await.expect("side agent");
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
    async fn peek_main_is_read_only_and_returns_main_messages() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = test_session(cwd.path());
        session
            .load_history(vec![
                Message::user_text("one", 1),
                Message::user_text("two", 2),
            ])
            .await
            .expect("history");
        let application = Application::new(session).await;
        let peek = application.peek_main_history(None).expect("peek");
        assert_eq!(peek.messages.len(), 2);
        let before = application.messages();
        let _ = application.peek_main_history(None).expect("peek again");
        assert_eq!(application.messages(), before);
    }

    #[tokio::test]
    async fn fork_scrubs_provider_hooks_and_refreshes_auth_with_independent_session_id() {
        let cwd = tempfile::tempdir().expect("cwd");
        let hook_calls = Arc::new(AtomicUsize::new(0));
        let resolver_calls = Arc::new(AtomicUsize::new(0));
        let captured = Arc::new(std::sync::Mutex::new(None));
        let mut stream_options = SimpleStreamOptions::default();
        stream_options.stream.transport = Transport::WebSocket;
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
                })
            })
        });
        let session = crate::Session::new(crate::SessionOptions {
            model: Model::default(),
            cwd: cwd.path().to_path_buf(),
            system_prompt: "main system".to_owned(),
            thinking_level: ThinkingLevel::Off,
            api_key: "stale-key".to_owned(),
            compaction: None,
            stream_options,
            tools: Some(crate::create_read_only_tools(&cwd.path().to_string_lossy())),
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: Some(stream_fn),
            auth_resolver: Some(resolver),
        })
        .expect("session");
        let main_options = session.stream_options();
        let main_session_id = main_options.stream.session_id.clone().expect("main session id");
        let application = Application::new(session).await;
        let (fork, agent) = application.open_side_chat_agent().await.expect("side agent");

        assert_ne!(fork.stream_options.stream.session_id.as_deref(), Some(main_session_id.as_str()));
        assert_eq!(fork.stream_options.stream.transport, Transport::WebSocket);
        assert_eq!(fork.stream_options.stream.timeout_ms, Some(12_345));
        assert_eq!(fork.stream_options.stream.headers.get("X-Static").map(String::as_str), Some("static"));
        assert_eq!(fork.stream_options.stream.env.get("STATIC_ENV").map(String::as_str), Some("static"));
        assert!(fork.stream_options.stream.on_payload.is_none());
        assert!(fork.stream_options.stream.on_response.is_none());
        assert!(fork.stream_options.stream.before_provider_request.is_none());
        assert!(fork.stream_options.stream.before_provider_headers.is_none());
        assert!(fork.stream_options.stream.after_provider_response.is_none());

        agent.prompt("side request").await.expect("side prompt");
        let observed = captured.lock().expect("captured options").clone().expect("stream options");
        assert_eq!(resolver_calls.load(Ordering::SeqCst), 1);
        assert_eq!(hook_calls.load(Ordering::SeqCst), 0);
        assert_eq!(observed.stream.api_key.as_deref(), Some("refreshed-key"));
        assert_ne!(observed.stream.api_key.as_deref(), Some("stale-key"));
        assert_eq!(observed.stream.headers.get("X-Static").map(String::as_str), Some("static"));
        assert_eq!(observed.stream.headers.get("x-refresh").map(String::as_str), Some("fresh"));
        assert!(!observed.stream.headers.contains_key("X-Refresh"));
        assert_eq!(observed.stream.headers.get("X-Auth").map(String::as_str), Some("auth"));
        assert_eq!(observed.stream.env.get("STATIC_ENV").map(String::as_str), Some("static"));
        assert_eq!(observed.stream.env.get("REFRESHED_ENV").map(String::as_str), Some("fresh"));
        assert_eq!(observed.stream.session_id, fork.stream_options.stream.session_id);
        assert!(observed.stream.on_payload.is_none());
        assert!(observed.stream.on_response.is_none());
        assert!(observed.stream.before_provider_request.is_none());
        assert!(observed.stream.before_provider_headers.is_none());
        assert!(observed.stream.after_provider_response.is_none());
    }

    #[tokio::test]
    async fn peek_main_uses_active_recorded_branch_and_entry_id_slicing() {
        let cwd = tempfile::tempdir().expect("cwd");
        let sessions = tempfile::tempdir().expect("sessions");
        let session = test_session(cwd.path());
        let recorder = crate::start_session_in(
            cwd.path(),
            Some(&Model::default()),
            Some("off"),
            Some(sessions.path()),
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
        session.record(recorder).expect("attach recorder");
        session
            .load_history(vec![
                Message::user_text("root", 1),
                Message::user_text("active", 3),
            ])
            .await
            .expect("active history");
        let application = Application::new(session).await;

        let full = application.peek_main_history(None).expect("full active branch");
        assert_eq!(full.leaf_id.as_deref(), Some(active.as_str()));
        assert_eq!(
            full.messages,
            vec![Message::user_text("root", 1), Message::user_text("active", 3)]
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
        let read_only =
            filter_tools_by_capabilities(tools, &[ToolCapability::Read]);
        assert!(!tools_include_mutation(&read_only));
        assert!(
            read_only
                .iter()
                .all(|tool| tool.capability == ToolCapability::Read)
        );
    }
}
