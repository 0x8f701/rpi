use std::{path::{Path, PathBuf}, sync::{Arc, Weak, atomic::{AtomicBool, Ordering}}, time::Duration};

use anyhow::{Result, anyhow};
use parking_lot::Mutex;
use pi_agent::{AgentEvent, Subscription, ThinkingLevel};
use pi_ai::{ContentBlock, CustomMessage, Message, Model};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::{sync::{Mutex as AsyncMutex, broadcast}, task::JoinHandle};

use crate::{
    CompactionReason, ExtensionActionHost, ExtensionCancellation, ExtensionCommandDescriptor,
    ExtensionContextSnapshot, ExtensionContextUsage, ExtensionEvent, ExtensionFuture,
    ExtensionInstanceId, ExtensionMessageDelivery, ExtensionPermissionSet, ExtensionRuntime,
    ExtensionRuntimeAction, MessageDelivery, ProcessEvent, ProcessId, ProcessInfo, ProcessKey,
    ProcessLogs, ProcessManager, ProcessOwnerId, ProcessSignal, ProcessSpawnSpec,
    ProcessTerminalSize, Session, SessionEvent,
};

const EVENT_BUFFER_CAPACITY: usize = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StreamingBehavior {
    Steer,
    FollowUp,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplicationState {
    pub model: Option<Model>,
    pub thinking_level: ThinkingLevel,
    pub is_streaming: bool,
    pub is_compacting: bool,
    pub steering_mode: pi_agent::QueueMode,
    pub follow_up_mode: pi_agent::QueueMode,
    pub session_file: Option<String>,
    pub session_id: Option<String>,
    pub session_name: Option<String>,
    pub auto_compaction_enabled: bool,
    pub message_count: usize,
    pub pending_message_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_selection: Option<crate::SelectionPlan>,
    pub todo_phases: Vec<crate::TodoPhase>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandSourceInfo {
    pub path: String,
    pub source: String,
    pub scope: String,
    pub origin: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_dir: Option<String>,
}

impl CommandSourceInfo {
    fn local(path: &Path, scope: &str) -> Self {
        Self {
            path: path.to_string_lossy().into_owned(),
            source: "local".to_owned(),
            scope: scope.to_owned(),
            origin: "top-level".to_owned(),
            base_dir: path.parent().map(|parent| parent.to_string_lossy().into_owned()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplicationCommand {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub source: String,
    pub source_info: CommandSourceInfo,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStartedEvent {
    #[serde(rename = "type")]
    pub record_type: String,
    pub version: u32,
    pub id: String,
    pub timestamp: String,
    pub cwd: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionBeforeTreeEvent {
    pub target_id: String,
    pub summarize: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionTreeEvent {
    pub target_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_leaf_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub editor_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary_entry_id: Option<String>,
    pub changed: bool,
    pub cancelled: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionBeforeForkEvent {
    pub target_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionForkedEvent {
    pub target_id: String,
    pub session_id: String,
    pub session_file: String,
    pub editor_text: String,
}


#[derive(Clone, Debug)]
pub enum ApplicationEvent {
    SessionStarted(SessionStartedEvent),
    Session(SessionEvent),
    Agent(AgentEvent),
    RunFailed { message: String },
    AgentSettled,
    Exported { path: String },
    ShareSucceeded { url: String },
    ShareFailed { message: String },
    SessionBeforeTree(SessionBeforeTreeEvent),
    SessionTree(SessionTreeEvent),
    SessionBeforeFork(SessionBeforeForkEvent),
    SessionForked(SessionForkedEvent),
    Process(ProcessEvent),
    Selection(crate::SelectionPlan),
    Loop(crate::LoopEvent),
    TodoUpdated {
        phases: Vec<crate::TodoPhase>,
        completed_tasks: Vec<crate::TodoCompletionTransition>,
    },
    TodoReminder {
        phases: Vec<crate::TodoPhase>,
    },
}

impl Serialize for ApplicationEvent {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::SessionStarted(event) => event.serialize(serializer),
            Self::Agent(event) => event.serialize(serializer),
            Self::Session(event) => event.serialize(serializer),
            Self::RunFailed { message } => {
                serde_json::json!({ "type": "run_failed", "message": message }).serialize(serializer)
            }
            Self::AgentSettled => {
                serde_json::json!({ "type": "agent_settled" }).serialize(serializer)
            }
            Self::Exported { path } => {
                serde_json::json!({ "type": "exported", "path": path }).serialize(serializer)
            }
            Self::ShareSucceeded { url } => {
                serde_json::json!({ "type": "share_succeeded", "url": url }).serialize(serializer)
            }
            Self::ShareFailed { message } => {
                serde_json::json!({ "type": "share_failed", "message": message }).serialize(serializer)
            }
            Self::SessionBeforeTree(event) => {
                serde_json::json!({ "type": "session_before_tree", "targetId": event.target_id, "summarize": event.summarize }).serialize(serializer)
            }
            Self::SessionTree(event) => {
                serde_json::json!({ "type": "session_tree", "targetId": event.target_id, "activeLeafId": event.active_leaf_id, "editorText": event.editor_text, "summaryEntryId": event.summary_entry_id, "changed": event.changed, "cancelled": event.cancelled }).serialize(serializer)
            }
            Self::SessionBeforeFork(event) => {
                serde_json::json!({ "type": "session_before_fork", "targetId": event.target_id }).serialize(serializer)
            }
            Self::SessionForked(event) => {
                serde_json::json!({ "type": "session_forked", "targetId": event.target_id, "sessionId": event.session_id, "sessionFile": event.session_file, "editorText": event.editor_text }).serialize(serializer)
            }
            Self::Process(event) => event.serialize(serializer),
            Self::Selection(plan) => {
                serde_json::json!({ "type": "selection", "selection": plan }).serialize(serializer)
            }
            Self::Loop(event) => event.serialize(serializer),
            Self::TodoUpdated { phases, completed_tasks } => {
                serde_json::json!({
                    "type": "todo_updated",
                    "phases": phases,
                    "completed_tasks": completed_tasks,
                })
                .serialize(serializer)
            }
            Self::TodoReminder { phases } => {
                serde_json::json!({ "type": "todo_reminder", "phases": phases })
                    .serialize(serializer)
            }
        }
    }
}

#[derive(Clone)]
pub struct Application {
    inner: Arc<ApplicationInner>,
}

struct ApplicationInner {
    session: Session,
    events: broadcast::Sender<ApplicationEvent>,
    session_subscription: Mutex<Option<Subscription>>,
    active_run: Mutex<Option<JoinHandle<()>>>,
    extension_runtime: Mutex<Option<(ExtensionRuntime, ExtensionPermissionSet)>>,
    orchestration_runtime: Mutex<Option<crate::OrchestrationRuntime>>,
    orchestration_explicit: AtomicBool,
    runtime_settings: Mutex<Arc<crate::RuntimeSettingsSnapshot>>,
    process_manager: ProcessManager,
    process_owner_id: ProcessOwnerId,
    process_events: Mutex<Option<JoinHandle<()>>>,
    session_events: Mutex<Option<JoinHandle<()>>>,
    loop_scheduler: Mutex<Option<crate::LoopSchedulerRuntime>>,
    turn_gate: Arc<AsyncMutex<()>>,
    loop_turn_active: AtomicBool,
    cleanup_lock: AsyncMutex<()>,
    cleaned: AtomicBool,
}

struct ApplicationExtensionHost {
    application: Weak<ApplicationInner>,
}

impl Application {
    pub async fn new(session: Session) -> Self {
        Self::build(session, None).await
    }

    pub async fn new_with_extensions(
        session: Session,
        runtime: ExtensionRuntime,
        permissions: ExtensionPermissionSet,
    ) -> Self {
        Self::build(session, Some((runtime, permissions))).await
    }

    pub async fn new_with_orchestration(
        session: Session,
        runtime: crate::OrchestrationRuntime,
    ) -> Self {
        let application = Self::build(session, None).await;
        *application.inner.orchestration_runtime.lock() = Some(runtime);
        application
            .inner
            .orchestration_explicit
            .store(true, Ordering::Release);
        application
    }

    async fn build(
        session: Session,
        extension_runtime: Option<(ExtensionRuntime, ExtensionPermissionSet)>,
    ) -> Self {
        let (events, _) = broadcast::channel(EVENT_BUFFER_CAPACITY);
        let process_manager = session.process_manager();
        let process_owner_id = session.process_owner_id();
        let runtime_settings = session
            .resource_manager()
            .map(|resources| resources.snapshot().settings.runtime_settings())
            .transpose()
            .expect("attached resource settings were already validated")
            .unwrap_or_else(|| crate::Settings::default().runtime_settings().expect("default settings"));
        let inner = Arc::new(ApplicationInner {
            session: session.clone(),
            events,
            active_run: Mutex::new(None),
            session_subscription: Mutex::new(None),
            session_events: Mutex::new(None),
            extension_runtime: Mutex::new(extension_runtime),
            orchestration_runtime: Mutex::new(None),
            orchestration_explicit: AtomicBool::new(false),
            runtime_settings: Mutex::new(Arc::new(runtime_settings)),
            process_manager: process_manager.clone(),
            process_owner_id,
            process_events: Mutex::new(None),
            loop_scheduler: Mutex::new(None),
            turn_gate: Arc::new(AsyncMutex::new(())),
            loop_turn_active: AtomicBool::new(false),
            cleanup_lock: AsyncMutex::new(()),
            cleaned: AtomicBool::new(false),
        });
        if let Some((runtime, _)) = inner.extension_runtime.lock().as_ref() {
            runtime
                .set_action_host(Arc::new(ApplicationExtensionHost {
                    application: Arc::downgrade(&inner),
                }))
                .expect("new extension runtime must not already have an action host");
            let before_start_runtime = runtime.clone();
            session.set_before_agent_start(Some(Arc::new(move |context| {
                let runtime = before_start_runtime.clone();
                Box::pin(async move {
                    let event = serde_json::json!({
                        "prompt": prompt_text(&context.messages),
                        "images": prompt_images(&context.messages),
                    });
                    let reduction = runtime
                        .reduce_before_agent_start(event, context.system_prompt)
                        .await?;
                    let mut messages = context.messages;
                    messages.extend(reduction.messages.into_iter().map(extension_custom_message));
                    Ok(pi_agent::BeforeAgentStartResult {
                        system_prompt: reduction.system_prompt,
                        messages,
                    })
                })
            })));
            let context_runtime = runtime.clone();
            session.set_transform_context(Some(Arc::new(move |messages, _| {
                let runtime = context_runtime.clone();
                Box::pin(async move { runtime.reduce_context(messages).await })
            })));
            let message_runtime = runtime.clone();
            session.set_transform_message(Some(Arc::new(move |message, _| {
                let runtime = message_runtime.clone();
                Box::pin(async move { runtime.reduce_message_end(message).await })
            })));
            let tool_call_runtime = runtime.clone();
            session.set_before_tool_call(Some(Arc::new(move |context| {
                let runtime = tool_call_runtime.clone();
                Box::pin(async move {
                    let reduction = runtime
                        .reduce_tool_call(
                            &context.tool_call.id,
                            &context.tool_call.name,
                            context.arguments,
                        )
                        .await?;
                    Ok(pi_agent::BeforeToolCallResult {
                        block: reduction.block,
                        reason: reduction.reason,
                        arguments: Some(reduction.input),
                    })
                })
            })));
            let tool_result_runtime = runtime.clone();
            session.set_after_tool_call(Some(Arc::new(move |context| {
                let runtime = tool_result_runtime.clone();
                Box::pin(async move {
                    let reduction = runtime
                        .reduce_tool_result(
                            &context.tool_call.id,
                            &context.tool_call.name,
                            context.arguments,
                            context.result.content,
                            Some(context.result.details),
                            context.is_error,
                        )
                        .await?;
                    Ok(pi_agent::AfterToolCallResult {
                        content: Some(reduction.content),
                        details: reduction.details,
                        is_error: Some(reduction.is_error),
                        usage: reduction.usage.map(extension_usage),
                        terminate: None,
                    })
                })
            })));
            let mut stream_options = session.stream_options();
            let request_runtime = runtime.clone();
            stream_options.stream.before_provider_request = Some(Arc::new(move |payload, _| {
                let runtime = request_runtime.clone();
                Box::pin(async move { runtime.reduce_provider_request(payload).await })
            }));
            let headers_runtime = runtime.clone();
            stream_options.stream.before_provider_headers = Some(Arc::new(move |headers, _| {
                let runtime = headers_runtime.clone();
                Box::pin(async move { runtime.reduce_provider_headers(headers).await })
            }));
            let response_runtime = runtime.clone();
            stream_options.stream.after_provider_response = Some(Arc::new(move |response, _| {
                let runtime = response_runtime.clone();
                Box::pin(async move {
                    runtime
                        .emit_checked(ExtensionEvent::new(
                            "after_provider_response",
                            serde_json::json!({ "status": response.status, "headers": response.headers }),
                        ))
                        .await
                })
            }));
            session.set_stream_options(stream_options).await;
            let compact_runtime = runtime.clone();
            session.set_before_compaction(Some(Arc::new(move |context| {
                let runtime = compact_runtime.clone();
                Box::pin(async move {
                    let reduction = runtime
                        .reduce_before_compact(serde_json::json!({
                            "customInstructions": context.custom_instructions,
                            "reason": context.reason,
                            "willRetry": context.will_retry,
                        }))
                        .await?;
                    Ok(crate::BeforeCompactionResult {
                        cancel: reduction.cancel,
                        compaction: reduction.compaction,
                    })
                })
            })));
        }
        let event_inner = Arc::downgrade(&inner);
        let subscription = session
            .subscribe(move |event| {
                let event_inner = event_inner.clone();
                async move {
                    let Some(inner) = event_inner.upgrade() else {
                        return Ok(());
                    };
                    let extension = inner
                        .extension_runtime
                        .lock()
                        .as_ref()
                        .map(|(runtime, _)| runtime.clone());
                    if let Some(runtime) = extension
                        && let Some(extension_event) = agent_extension_event(&event)
                    {
                        let _ = runtime.emit(extension_event).await;
                    }
                    if matches!(&event, AgentEvent::ToolExecutionEnd { tool_name, is_error: false, .. } if tool_name == "todo") {
                        let state = inner.session.todo_state();
                        let completed_tasks = match &event {
                            AgentEvent::ToolExecutionEnd { result, .. } => serde_json::from_value::<crate::TodoToolDetails>(result.details.clone())
                                .map(|details| details.completed_tasks)
                                .unwrap_or_default(),
                            _ => Vec::new(),
                        };
                        inner.publish(ApplicationEvent::TodoUpdated {
                            phases: state.phases,
                            completed_tasks,
                        });
                    }
                    inner.publish(ApplicationEvent::Agent(event));
                    Ok(())
                }
            })
            .await;
        *inner.session_subscription.lock() = Some(subscription);
        let mut session_events = session.subscribe_session_events();
        let session_event_inner = Arc::downgrade(&inner);
        let session_event_task = tokio::spawn(async move {
            while let Ok(event) = session_events.recv().await {
                let Some(inner) = session_event_inner.upgrade() else {
                    break;
                };
                let extension = session_extension_event(&event);
                let runtime = inner
                    .extension_runtime
                    .lock()
                    .as_ref()
                    .map(|(runtime, _)| runtime.clone());
                if let (Some(extension), Some(runtime)) = (extension, runtime) {
                    let _ = runtime.emit(extension).await;
                }
                inner.publish(ApplicationEvent::Session(event));
            }
        });
        *inner.session_events.lock() = Some(session_event_task);
        let mut process_events = process_manager.subscribe();
        let process_event_inner = Arc::downgrade(&inner);
        let process_event_task = tokio::spawn(async move {
            loop {
                match process_events.recv().await {
                    Ok(event) => {
                        let Some(inner) = process_event_inner.upgrade() else {
                            break;
                        };
                        if process_event_owner(&event) == &inner.process_owner_id {
                            inner.publish(ApplicationEvent::Process(event));
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        *inner.process_events.lock() = Some(process_event_task);
        let loop_runner_inner = Arc::downgrade(&inner);
        let loop_runner: crate::LoopTurnRunner = Arc::new(move |request, cancel| {
            let inner = loop_runner_inner.clone();
            Box::pin(async move { run_loop_turn(inner, request, cancel).await })
        });
        let loop_event_inner = Arc::downgrade(&inner);
        let loop_events: crate::LoopEventSink = Arc::new(move |event| {
            if let Some(inner) = loop_event_inner.upgrade() {
                inner.publish(ApplicationEvent::Loop(event));
            }
        });
        let session_file = session.recorder_info().map(|(_, path)| path);
        let loop_scheduler = crate::start_loop_scheduler(
            session_file.as_deref(),
            loop_runner,
            loop_events,
        );
        *inner.loop_scheduler.lock() = Some(loop_scheduler);
        Self { inner }
    }

    #[must_use]
    pub fn extension_runtime(&self) -> Option<ExtensionRuntime> {
        self.inner
            .extension_runtime
            .lock()
            .as_ref()
            .map(|(runtime, _)| runtime.clone())
    }


    #[must_use]
    pub fn runtime_settings(&self) -> Arc<crate::RuntimeSettingsSnapshot> {
        self.inner.runtime_settings.lock().clone()
    }

    #[must_use]
    pub fn runtime_settings_state(&self) -> crate::RuntimeSettingsState {
        self.runtime_settings().state()
    }
    #[must_use]
    pub fn orchestration_runtime(&self) -> Option<crate::OrchestrationRuntime> {
        self.inner.orchestration_runtime.lock().as_ref().cloned()
    }

    pub fn attach_orchestration(&self, runtime: crate::OrchestrationRuntime) -> Result<()> {
        self.attach_orchestration_with_override(runtime, true)
    }

    pub fn attach_orchestration_with_override(
        &self,
        runtime: crate::OrchestrationRuntime,
        explicit: bool,
    ) -> Result<()> {
        let mut current = self.inner.orchestration_runtime.lock();
        if current.is_some() {
            return Err(anyhow!("application orchestration is already configured"));
        }
        *current = Some(runtime);
        self.inner.orchestration_explicit.store(explicit, Ordering::Release);
        Ok(())
    }

    #[must_use]
    pub fn session(&self) -> Session {
        self.inner.session.clone()
    }

    #[must_use]
    pub fn get_active_tool_names(&self) -> Vec<String> {
        self.inner.session.get_active_tool_names()
    }

    #[must_use]
    pub fn get_all_tools(&self) -> Vec<pi_agent::AgentTool> {
        self.inner.session.get_all_tools()
    }

    #[must_use]
    pub fn get_tool_definition(&self, name: &str) -> Option<pi_agent::AgentTool> {
        self.inner.session.get_tool_definition(name)
    }

    pub async fn set_active_tools_by_name(&self, names: &[String]) -> Result<()> {
        self.inner.session.set_active_tools_by_name(names).await
    }

    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<ApplicationEvent> {
        self.inner.events.subscribe()
    }

    #[must_use]
    pub fn todo_state(&self) -> crate::TodoState {
        self.inner.session.todo_state()
    }

    pub fn apply_todo(&self, op: crate::TodoOp) -> Result<crate::TodoApplyResult> {
        match self.inner.session.apply_todo(op) {
            Ok(result) => {
                self.inner.publish(ApplicationEvent::TodoUpdated {
                    phases: result.phases.clone(),
                    completed_tasks: result.completed_tasks.clone(),
                });
                Ok(result)
            }
            Err(error) => {
                self.inner.session.schedule_todo_reminder();
                Err(error)
            }
        }
    }

    pub fn set_todos(&self, phases: Vec<crate::TodoPhase>) -> Result<crate::TodoApplyResult> {
        let result = self.inner.session.set_todos(phases)?;
        self.inner.publish(ApplicationEvent::TodoUpdated {
            phases: result.phases.clone(),
            completed_tasks: result.completed_tasks.clone(),
        });
        Ok(result)
    }
    pub async fn loop_create(
        &self,
        request: crate::LoopCreateRequest,
    ) -> Result<crate::LoopTask> {
        Ok(self.loop_handle()?.create(request).await?)
    }

    pub async fn loop_update(
        &self,
        request: crate::LoopUpdateRequest,
    ) -> Result<crate::LoopTask> {
        Ok(self.loop_handle()?.update(request).await?)
    }

    pub async fn loop_list(&self) -> Result<Vec<crate::LoopTask>> {
        Ok(self.loop_handle()?.list().await?)
    }

    pub async fn loop_delete(&self, task_id: &str) -> Result<bool> {
        Ok(self.loop_handle()?.delete(task_id).await?)
    }

    pub async fn loop_cancel(&self, task_id: &str) -> Result<bool> {
        Ok(self.loop_handle()?.cancel(task_id).await?)
    }

    fn loop_handle(&self) -> Result<crate::LoopSchedulerHandle> {
        self.inner
            .loop_scheduler
            .lock()
            .as_ref()
            .map(|runtime| runtime.handle.clone())
            .ok_or_else(|| anyhow!("loop scheduler is unavailable"))
    }

    #[must_use]
    pub fn process_manager(&self) -> ProcessManager {
        self.inner.process_manager.clone()
    }

    pub async fn process_spawn(&self, spec: ProcessSpawnSpec) -> Result<ProcessInfo> {
        self.inner.process_manager.spawn(self.inner.process_owner_id.clone(), spec).await
    }

    #[must_use]
    pub fn process_list(&self) -> Vec<ProcessInfo> {
        self.inner.process_manager.list(&self.inner.process_owner_id)
    }

    pub fn process_describe(&self, id: &ProcessId) -> Result<ProcessInfo> {
        self.inner.process_manager.describe(&self.inner.process_owner_id, id)
    }

    pub async fn process_logs(&self, id: &ProcessId, cursor: u64, max_bytes: Option<usize>, follow: bool, timeout: Option<Duration>) -> Result<ProcessLogs> {
        self.inner.process_manager.logs(&self.inner.process_owner_id, id, cursor, max_bytes, follow, timeout).await
    }

    pub async fn process_write(&self, id: &ProcessId, bytes: Vec<u8>, close_stdin: bool) -> Result<()> {
        self.inner.process_manager.write(&self.inner.process_owner_id, id, bytes, close_stdin).await
    }

    pub async fn process_send_keys(&self, id: &ProcessId, keys: &[ProcessKey]) -> Result<()> {
        self.inner.process_manager.send_keys(&self.inner.process_owner_id, id, keys).await
    }

    pub fn process_resize(&self, id: &ProcessId, size: ProcessTerminalSize) -> Result<()> {
        self.inner.process_manager.resize(&self.inner.process_owner_id, id, size)
    }

    pub fn process_signal(&self, id: &ProcessId, signal: ProcessSignal) -> Result<()> {
        self.inner.process_manager.signal(&self.inner.process_owner_id, id, signal)
    }

    pub async fn process_stop(&self, id: &ProcessId, grace: Option<Duration>) -> Result<ProcessInfo> {
        self.inner.process_manager.stop(&self.inner.process_owner_id, id, grace).await
    }

    pub async fn process_wait(&self, id: &ProcessId, timeout: Option<Duration>) -> Result<ProcessInfo> {
        self.inner.process_manager.wait(&self.inner.process_owner_id, id, timeout).await
    }

    #[must_use]
    pub fn session_header(&self) -> Option<SessionStartedEvent> {
        self.inner.session.session_header().map(|header| SessionStartedEvent {
            record_type: header.record_type,
            version: header.version,
            id: header.id,
            timestamp: header.timestamp,
            cwd: header.cwd.to_string_lossy().into_owned(),
        })
    }

    #[must_use]
    pub fn messages(&self) -> Vec<Message> {
        self.inner.session.history()
    }

    #[must_use]
    pub fn last_assistant_text(&self) -> Option<String> {
        let text = self.inner.session.last_assistant_text();
        (!text.is_empty()).then_some(text)
    }

    #[must_use]
    pub async fn state(&self) -> ApplicationState {
        let session = &self.inner.session;
        let (session_id, session_file) = session
            .recorder_info()
            .map_or((None, None), |(id, path)| {
                (Some(id), Some(path.to_string_lossy().into_owned()))
            });
        ApplicationState {
            model: session.model(),
            thinking_level: session.thinking_level(),
            is_streaming: self.is_streaming(),
            is_compacting: session.is_compacting(),
            steering_mode: session.steering_mode().await,
            follow_up_mode: session.follow_up_mode().await,
            session_file,
            session_id,
            session_name: session.session_name(),
            auto_compaction_enabled: session.auto_compaction_enabled(),
            message_count: session.history().len(),
            pending_message_count: session.pending_message_count().await,
            last_selection: session.last_selection(),
            todo_phases: session.todo_state().phases,
        }
    }

    #[must_use]
    pub fn is_streaming(&self) -> bool {
        self.inner.loop_turn_active.load(Ordering::Acquire)
            || self
                .inner
                .active_run
                .lock()
                .as_ref()
                .is_some_and(|run| !run.is_finished())
    }

    pub async fn prompt(
        &self,
        message: String,
        images: Vec<ContentBlock>,
        streaming_behavior: Option<StreamingBehavior>,
    ) -> Result<()> {
        let (message, images) = if let Some(runtime) = self.extension_runtime() {
            match runtime
                .reduce_input(
                    message,
                    images,
                    "interactive",
                    streaming_behavior.map(streaming_behavior_name),
                )
                .await?
            {
                crate::ExtensionInputReduction::Continue { text, images } => (text, images),
                crate::ExtensionInputReduction::Handled => return Ok(()),
            }
        } else {
            (message, images)
        };
        if self.inner.loop_turn_active.load(Ordering::Acquire) {
            return match streaming_behavior {
                Some(StreamingBehavior::Steer) => {
                    self.inner.session.steer(user_message(message, images)).await;
                    Ok(())
                }
                Some(StreamingBehavior::FollowUp) => {
                    self.inner.session.follow_up(user_message(message, images)).await;
                    Ok(())
                }
                None => Err(anyhow!(
                    "session is already processing; choose steer or followUp"
                )),
            };
        }
        let Ok(turn_guard) = self.inner.turn_gate.clone().try_lock_owned() else {
            return match streaming_behavior {
                Some(StreamingBehavior::Steer) => {
                    self.inner.session.steer(user_message(message, images)).await;
                    Ok(())
                }
                Some(StreamingBehavior::FollowUp) => {
                    self.inner.session.follow_up(user_message(message, images)).await;
                    Ok(())
                }
                None => Err(anyhow!(
                    "session is already processing; choose steer or followUp"
                )),
            };
        };
        let session = self.inner.session.clone();
        let inner = self.inner.clone();
        let selection = session.select_for_request(&message).await;
        inner.publish(ApplicationEvent::Selection(selection));
        if session.todo_reminder_pending() {
            inner.publish(ApplicationEvent::TodoReminder {
                phases: session.todo_state().phases,
            });
        }
        let turn_guard = turn_guard;
        let handle = tokio::spawn(async move {
            if let Err(error) = session.run(&message, images).await {
                inner.publish(ApplicationEvent::RunFailed {
                    message: error.to_string(),
                });
            }
            inner.publish(ApplicationEvent::AgentSettled);
            drop(turn_guard);
        });
        *self.inner.active_run.lock() = Some(handle);
        Ok(())
    }

    pub async fn steer(&self, message: String, images: Vec<ContentBlock>) {
        self.inner.session.steer(user_message(message, images)).await;
    }

    pub async fn follow_up(&self, message: String, images: Vec<ContentBlock>) {
        self.inner.session.follow_up(user_message(message, images)).await;
    }

    pub async fn abort(&self) {
        if let Some(runtime) = self.orchestration_runtime() {
            runtime.cancel_active();
        }
        self.inner.session.abort().await;
    }

    pub fn abort_compaction(&self) {
        self.inner.session.abort_compaction();
    }

    pub fn set_auto_retry_enabled(&self, enabled: bool) {
        self.inner.session.set_auto_retry_enabled(enabled);
    }

    pub fn abort_retry(&self) {
        self.inner.session.abort_retry();
    }

    pub async fn execute_bash(
        &self,
        command: String,
        exclude_from_context: bool,
    ) -> Result<crate::BashResult> {
        self.execute_bash_with_id(command, exclude_from_context, None)
            .await
    }

    pub async fn execute_bash_with_id(
        &self,
        command: String,
        exclude_from_context: bool,
        id: Option<String>,
    ) -> Result<crate::BashResult> {
        if let Some(runtime) = self.extension_runtime() {
            let reduction = runtime
                .reduce_user_bash(serde_json::json!({
                    "command": command,
                    "excludeFromContext": exclude_from_context,
                    "cwd": self.inner.session.cwd(),
                }))
                .await?;
            if let Some(result) = reduction.result {
                self.inner
                    .session
                    .record_bash_result(&command, exclude_from_context, &result)
                    .await?;
                return Ok(result);
            }
        }
        self.inner
            .session
            .execute_bash_with_id(&command, exclude_from_context, id)
            .await
    }

    pub fn abort_bash(&self) {
        self.inner.session.abort_bash();
    }

    #[must_use]
    pub fn is_bash_running(&self) -> bool {
        self.inner.session.is_bash_running()
    }

    #[must_use]
    pub fn session_stats(&self) -> crate::SessionStats {
        self.inner.session.session_stats()
    }

    pub async fn queued_messages(&self) -> (Vec<Message>, Vec<Message>) {
        self.inner.session.queued_messages().await
    }

    pub async fn drain_queued_messages(&self) -> (Vec<Message>, Vec<Message>) {
        self.inner.session.drain_queued_messages().await
    }

    pub async fn wait_for_idle(&self) {
        self.inner.session.wait_for_idle().await;
        let handle = self.inner.active_run.lock().take();
        if let Some(handle) = handle {
            let _ = handle.await;
        }
    }

    pub async fn new_session(&self) -> Result<()> {
        self.new_session_with_parent(None).await
    }

    pub async fn new_session_with_parent(&self, parent_session: Option<&Path>) -> Result<()> {
        let loops = self.loop_handle()?;
        let previous = self.inner.session.recorder_info().map(|(_, path)| path);
        loops.suspend(crate::LoopRemovalReason::SessionChanged).await?;
        self.wait_for_idle().await;
        self.inner
            .process_manager
            .shutdown_owner(&self.inner.process_owner_id)
            .await;
        let result = self
            .inner
            .session
            .reset()
            .await
            .and_then(|()| self.inner.session.start_new_recording_with_parent(parent_session));
        if let Err(error) = result {
            let _ = loops.activate(previous).await;
            return Err(error);
        }
        let current = self.inner.session.recorder_info().map(|(_, path)| path);
        loops.activate(current).await?;
        Ok(())
    }

    pub async fn compact(
        &self,
        custom_instructions: Option<&str>,
    ) -> Result<crate::CompactionResult> {
        self.wait_for_idle().await;
        self.inner.session.compact(custom_instructions).await
    }
    pub async fn switch_session(&self, path: &Path) -> Result<()> {
        let loops = self.loop_handle()?;
        let previous = self.inner.session.recorder_info().map(|(_, path)| path);
        loops.suspend(crate::LoopRemovalReason::SessionChanged).await?;
        self.wait_for_idle().await;
        self.inner
            .process_manager
            .shutdown_owner(&self.inner.process_owner_id)
            .await;
        if let Err(error) = self.inner.session.switch_session(path).await {
            let _ = loops.activate(previous).await;
            return Err(error);
        }
        let current = self.inner.session.recorder_info().map(|(_, path)| path);
        loops.activate(current).await?;
        Ok(())
    }

    pub async fn fork_session(&self, entry_id: &str) -> Result<String> {
        let loops = self.loop_handle()?;
        let previous = self.inner.session.recorder_info().map(|(_, path)| path);
        loops.suspend(crate::LoopRemovalReason::SessionChanged).await?;
        self.wait_for_idle().await;
        self.inner
            .process_manager
            .shutdown_owner(&self.inner.process_owner_id)
            .await;
        self.inner.publish(ApplicationEvent::SessionBeforeFork(SessionBeforeForkEvent { target_id: entry_id.to_owned() }));
        let editor_text = match self.inner.session.fork_session(entry_id).await {
            Ok(editor_text) => editor_text,
            Err(error) => {
                let _ = loops.activate(previous).await;
                return Err(error);
            }
        };
        let (session_id, session_file) = self.inner.session.recorder_info().ok_or_else(|| anyhow!("forked session recording is unavailable"))?;
        loops.activate(Some(session_file.clone())).await?;
        self.inner.publish(ApplicationEvent::SessionForked(SessionForkedEvent { target_id: entry_id.to_owned(), session_id, session_file: session_file.to_string_lossy().into_owned(), editor_text: editor_text.clone() }));
        Ok(editor_text)
    }

    pub async fn navigate_tree(&self, entry_id: &str, options: crate::NavigateTreeOptions) -> Result<crate::NavigateTreeResult> {
        self.wait_for_idle().await;
        self.inner.publish(ApplicationEvent::SessionBeforeTree(SessionBeforeTreeEvent { target_id: entry_id.to_owned(), summarize: options.summarize }));
        let result = self.inner.session.navigate_tree(entry_id, options).await?;
        self.inner.publish(ApplicationEvent::SessionTree(SessionTreeEvent { target_id: entry_id.to_owned(), active_leaf_id: result.active_leaf_id.clone(), editor_text: result.editor_text.clone(), summary_entry_id: result.summary_entry_id.clone(), changed: result.changed, cancelled: result.cancelled }));
        Ok(result)
    }

    pub fn set_session_label(&self, target_id: &str, label: Option<&str>) -> Result<()> {
        self.inner.session.set_session_label(target_id, label)
    }

    pub async fn clone_session(&self) -> Result<()> {
        let loops = self.loop_handle()?;
        let previous = self.inner.session.recorder_info().map(|(_, path)| path);
        loops.suspend(crate::LoopRemovalReason::SessionChanged).await?;
        self.wait_for_idle().await;
        self.inner
            .process_manager
            .shutdown_owner(&self.inner.process_owner_id)
            .await;
        if let Err(error) = self.inner.session.clone_session().await {
            let _ = loops.activate(previous).await;
            return Err(error);
        }
        let current = self.inner.session.recorder_info().map(|(_, path)| path);
        loops.activate(current).await?;
        Ok(())
    }

    pub fn fork_messages(&self) -> Result<Vec<crate::ForkMessage>> {
        self.inner.session.fork_messages()
    }

    pub fn session_entries(&self, since: Option<&str>) -> Result<crate::SessionEntries> {
        self.inner.session.session_entries(since)
    }

    pub fn session_tree(&self) -> Result<crate::SessionTreeResult> {
        self.inner.session.session_tree()
    }

    pub fn export_html(&self, output: Option<&Path>) -> Result<PathBuf> {
        crate::export_live_session(&self.inner.session, output, &crate::ExportOptions::default())
    }

    pub fn export_jsonl(&self, output: Option<&Path>) -> Result<PathBuf> {
        let (_, path) = self
            .inner
            .session
            .recorder_info()
            .ok_or_else(|| anyhow!("session recording is unavailable"))?;
        crate::export_session_jsonl(&path, output)
    }


    /// Public `/reload` adapter over the session resource snapshot and extension layer.
    pub fn commands_catalog(&self) -> Vec<crate::ApplicationCommand> {
        let mut commands = Vec::new();
        if let Some(runtime) = self.extension_runtime() {
            commands.extend(runtime.command_sources().into_iter().map(|source| crate::ApplicationCommand {
                name: source.command.name,
                description: source.command.description,
                source: "extension".to_owned(),
                source_info: crate::CommandSourceInfo {
                    path: source.path.to_string_lossy().into_owned(),
                    source: source.source,
                    scope: source.scope,
                    origin: source.origin,
                    base_dir: source.base_dir.map(|path| path.to_string_lossy().into_owned()),
                },
            }));
        }
        if let Some(snapshot) = self.resource_snapshot() {
            commands.extend(snapshot.prompts.iter().map(|prompt| crate::ApplicationCommand {
                name: prompt.name.clone(),
                description: (!prompt.description.is_empty()).then(|| prompt.description.clone()),
                source: "prompt".to_owned(),
                source_info: crate::CommandSourceInfo::local(
                    &prompt.file_path,
                    match prompt.scope {
                        crate::ResourceScope::Global => "user",
                        crate::ResourceScope::Project => "project",
                        crate::ResourceScope::Explicit => "temporary",
                    },
                ),
            }));
            commands.extend(snapshot.skills.iter().map(|skill| crate::ApplicationCommand {
                name: format!("skill:{}", skill.name),
                description: (!skill.description.is_empty()).then(|| skill.description.clone()),
                source: "skill".to_owned(),
                source_info: crate::CommandSourceInfo {
                    path: skill.file_path.clone(),
                    source: "local".to_owned(),
                    scope: if skill.file_path.starts_with(&snapshot.cwd.to_string_lossy().into_owned()) {
                        "project"
                    } else {
                        "user"
                    }
                    .to_owned(),
                    origin: "top-level".to_owned(),
                    base_dir: Some(skill.base_dir.clone()),
                },
            }));
        }
        commands
    }

    fn orchestration_candidate(
        &self,
        snapshot: &crate::ResourceSnapshot,
        settings: &crate::RuntimeSettingsSnapshot,
    ) -> Result<Option<crate::OrchestrationRuntime>> {
        let enabled = settings.orchestration_enabled
            || self.inner.orchestration_explicit.load(Ordering::Acquire);
        if !enabled {
            return Ok(None);
        }
        let mut config = crate::OrchestrationConfig::new(
            crate::AgentCatalog::from_agents(snapshot.agents.clone()),
            snapshot.cwd.join(".pi").join("artifacts"),
        );
        config.skills = snapshot.skills.iter().map(crate::OrchestrationSkill::from).collect();
        config.max_concurrency = settings.orchestration_max_concurrency;
        config.max_recursion_depth = settings.orchestration_max_recursion_depth;
        config.mailbox_capacity = settings.orchestration_mailbox_capacity;
        config.max_tools_per_agent = settings.orchestration_max_tools_per_agent;
        if let Some(selector) = snapshot.settings.selector.clone() {
            config = config.with_selector_settings(selector);
        }
        let resolver = crate::OrchestrationRuntime::read_uri_resolver_for_artifact_dir(
            &config.artifact_dir,
        )?;
        let factory = crate::OrchestrationRuntime::child_factory_from_snapshot_and_uri(
            self.inner.session.child_session_options_snapshot(),
            Some(resolver),
        );
        Ok(Some(crate::OrchestrationRuntime::new(config, factory)?))
    }

    async fn commit_orchestration_candidate(
        &self,
        candidate: Option<crate::OrchestrationRuntime>,
    ) {
        let previous = {
            let mut current = self.inner.orchestration_runtime.lock();
            std::mem::replace(&mut *current, candidate)
        };
        if let Some(previous) = previous {
            previous.shutdown().await;
        }
    }

    /// The resource snapshot, extension registry, and agent tool set are all staged
    /// before the active resource and extension generations are replaced.
    pub async fn reload(&self) -> Result<crate::ReloadResult> {
        self.wait_for_idle().await;
        let resources = self
            .inner
            .session
            .resource_manager()
            .ok_or_else(|| anyhow!("session has no resource manager"))?;
        let resource_candidate = resources.stage_reload()?;
        let next_runtime_settings = Arc::new(resource_candidate.snapshot().settings.runtime_settings()?);
        let orchestration_candidate =
            self.orchestration_candidate(&resource_candidate.snapshot(), &next_runtime_settings)?;
        let extension_runtime = self.inner.extension_runtime.lock().clone();
        let Some((runtime, permissions)) = extension_runtime else {
            let mut additional_tools = Vec::new();
            if let Some(orchestration) = &orchestration_candidate {
                additional_tools.extend(orchestration.agent_tools("Main", 0));
            }
            let update = self
                .inner
                .session
                .prepare_resource_update(resource_candidate.snapshot(), additional_tools)?;
            let result = resources.commit_reload(resource_candidate)?;
            self.inner.session.commit_resource_update(update).await;
            *self.inner.runtime_settings.lock() = next_runtime_settings;
            self.commit_orchestration_candidate(orchestration_candidate).await;
            return Ok(result);
        };
        let specs = resource_candidate.extension_specs(&permissions)?;
        let extension_candidate = runtime.stage_reload(specs).await;
        let report = extension_candidate.report();
        if !report.failures.is_empty() {
            runtime.discard_reload(extension_candidate).await;
            return Err(extension_load_report_error("extension reload", &report));
        }
        let mut additional_tools = extension_candidate.agent_tools(&runtime);
        if let Some(orchestration) = &orchestration_candidate {
            additional_tools.extend(orchestration.agent_tools("Main", 0));
        }
        let update = match self.inner.session.prepare_resource_update(
            resource_candidate.snapshot(),
            additional_tools,
        ) {
            Ok(update) => update,
            Err(error) => {
                runtime.discard_reload(extension_candidate).await;
                return Err(error);
            }
        };
        if let Err(error) = runtime.prepare_reload(&extension_candidate).await {
            runtime.discard_reload(extension_candidate).await;
            return Err(error);
        }
        let result = resources.commit_reload(resource_candidate)?;
        runtime.commit_reload(extension_candidate)?;
        runtime.finish_reload("reload").await;
        self.inner.session.commit_resource_update(update).await;
        *self.inner.runtime_settings.lock() = next_runtime_settings;
        self.commit_orchestration_candidate(orchestration_candidate).await;
        Ok(result)
    }

    #[must_use]
    pub fn resource_generation(&self) -> Option<u64> {
        self.inner
            .session
            .resource_manager()
            .map(|resources| resources.generation())
    }

    #[must_use]
    pub fn resource_snapshot(&self) -> Option<Arc<crate::ResourceSnapshot>> {
        self.inner
            .session
            .resource_manager()
            .map(|resources| resources.snapshot())
    }

    pub async fn set_steering_mode(&self, mode: pi_agent::QueueMode) {
        self.inner.session.set_steering_mode(mode).await;
    }

    pub async fn set_follow_up_mode(&self, mode: pi_agent::QueueMode) {
        self.inner.session.set_follow_up_mode(mode).await;
    }

    pub fn set_auto_compaction_enabled(&self, enabled: bool) {
        self.inner.session.set_auto_compaction_enabled(enabled);
    }

    pub fn set_session_name(&self, name: &str) -> Result<()> {
        self.inner.session.set_session_name(name)
    }

    pub async fn set_project_trust(
        &self,
        decision: crate::TrustDecision,
    ) -> Result<crate::ReloadResult> {
        let resources = self
            .inner
            .session
            .resource_manager()
            .ok_or_else(|| anyhow!("session has no resource manager"))?;
        resources
            .trust_store()
            .set(self.inner.session.cwd(), decision)?;
        self.reload().await
    }

    pub fn set_model(&self, model: Model, api_key: String) {
        self.inner.session.set_model(model, api_key);
    }

    pub async fn set_model_with_resolved_auth(&self, model: Model) -> Result<()> {
        self.inner.session.set_model_with_resolved_auth(model).await
    }

    pub fn set_thinking_level(&self, level: ThinkingLevel) {
        self.inner.session.set_thinking_level(level);
    }

    /// Stops every runtime owned by this application. Safe to call repeatedly;
    /// concurrent callers wait for the in-flight cleanup to finish.
    pub async fn cleanup(&self) {
        let _cleanup = self.inner.cleanup_lock.lock().await;
        if self.inner.cleaned.load(Ordering::Acquire) {
            return;
        }

        self.inner.session.abort().await;
        self.wait_for_idle().await;

        let loop_scheduler = self.inner.loop_scheduler.lock().take();
        if let Some(runtime) = loop_scheduler {
            runtime.shutdown().await;
        }
        let orchestration_runtime = self.inner.orchestration_runtime.lock().take();
        if let Some(runtime) = orchestration_runtime {
            runtime.shutdown().await;
        }

        self.inner
            .process_manager
            .shutdown_owner(&self.inner.process_owner_id)
            .await;
        let process_events = self.inner.process_events.lock().take();
        if let Some(task) = process_events {
            task.abort();
        }
        let session_events = self.inner.session_events.lock().take();
        if let Some(task) = session_events {
            task.abort();
        }
        self.inner.session_subscription.lock().take();

        let runtime = self
            .inner
            .extension_runtime
            .lock()
            .take()
            .map(|(runtime, _)| runtime);
        if let Some(runtime) = runtime {
            runtime.shutdown_with_reason("cleanup").await;
        }
        self.inner.cleaned.store(true, Ordering::Release);
    }

    /// Export the current live session to a self-contained HTML file.
    ///
    /// Prefers the persisted session file (preserving compaction markers)
    /// and falls back to in-memory history. Publishes an [`Exported`] event
    /// with the output path, or a [`ShareFailed`] event on error.
    pub fn export_session(&self, output: Option<PathBuf>) {
        let session = self.inner.session.clone();
        let events = self.inner.events.clone();
        let options = crate::export::ExportOptions::default();
        tokio::spawn(async move {
            let result = crate::export::export_live_session(&session, output.as_deref(), &options);
            match result {
                Ok(path) => {
                    let _ = events.send(ApplicationEvent::Exported {
                        path: path.display().to_string(),
                    });
                }
                Err(error) => {
                    let _ = events.send(ApplicationEvent::ShareFailed {
                        message: format!("export failed: {error}"),
                    });
                }
            }
        });
    }

    /// Share the current live session to a private GitHub gist.
    ///
    /// Exports the session to HTML, uploads it via `gh gist create
    /// --private`, and publishes a [`ShareSucceeded`] event with the viewer
    /// URL — or a [`ShareFailed`] event with an actionable error message.
    pub fn share_session(&self) {
        let session = self.inner.session.clone();
        let events = self.inner.events.clone();
        let options = crate::export::ExportOptions::default();
        tokio::spawn(async move {
            let result = crate::share::share_session(&session, &options).await;
            match result {
                Ok(share) => {
                    let _ = events.send(ApplicationEvent::ShareSucceeded {
                        url: share.viewer_url,
                    });
                }
                Err(error) => {
                    let _ = events.send(ApplicationEvent::ShareFailed {
                        message: error.to_string(),
                    });
                }
            }
        });
    }
}

impl ApplicationExtensionHost {
    fn application(&self) -> Result<Application> {
        self.application
            .upgrade()
            .map(|inner| Application { inner })
            .ok_or_else(|| anyhow!("application is shutting down"))
    }
}

impl ExtensionActionHost for ApplicationExtensionHost {
    fn context_snapshot(&self) -> ExtensionFuture<'_, Result<ExtensionContextSnapshot>> {
        Box::pin(async move {
            let application = self.application()?;
            let state = application.state().await;
            let stats = application.inner.session.session_stats();
            let project_trusted = application
                .resource_snapshot()
                .is_none_or(|snapshot| snapshot.trust.is_trusted());
            let commands = application
                .commands_catalog()
                .into_iter()
                .map(|command| ExtensionCommandDescriptor {
                    name: command.name,
                    description: command.description,
                })
                .collect();
            let all_tools = application
                .get_all_tools()
                .into_iter()
                .map(|tool| tool.name)
                .collect();
            let context_usage = stats.context_usage.map(|usage| ExtensionContextUsage {
                tokens: usage.tokens,
                context_window: usage.context_window,
                percent: usage.percent,
            });
            Ok(ExtensionContextSnapshot {
                session_name: state.session_name,
                session_id: state.session_id,
                session_file: state.session_file,
                is_idle: !state.is_streaming && !state.is_compacting,
                project_trusted,
                has_pending_messages: state.pending_message_count > 0,
                context_usage,
                active_tools: application.get_active_tool_names(),
                all_tools,
                commands,
                system_prompt: application.inner.session.current_system_prompt().await,
                model: state.model,
                thinking_level: state.thinking_level,
            })
        })
    }

    fn request(
        &self,
        _instance: ExtensionInstanceId,
        action: ExtensionRuntimeAction,
        cancellation: ExtensionCancellation,
    ) -> ExtensionFuture<'_, Result<Value>> {
        Box::pin(async move {
            let application = self.application()?;
            if cancellation.is_cancelled() {
                return Err(anyhow!("extension action was cancelled"));
            }
            match action {
                ExtensionRuntimeAction::SendMessage { message, delivery, trigger_turn } => {
                    application
                        .inner
                        .session
                        .send_custom_message(
                            CustomMessage {
                                custom_type: message.custom_type,
                                content: message.content,
                                display: message.display,
                                details: message.details,
                                timestamp: pi_ai::now_millis(),
                            },
                            message_delivery(delivery),
                            trigger_turn,
                        )
                        .await?;
                    Ok(Value::Null)
                }
                ExtensionRuntimeAction::SendUserMessage { content, delivery } => {
                    application
                        .inner
                        .session
                        .send_user_message(content, message_delivery(delivery))
                        .await?;
                    Ok(Value::Null)
                }
                ExtensionRuntimeAction::AppendEntry { custom_type, data } => {
                    serde_json::to_value(application.inner.session.append_custom_entry(&custom_type, data)?)
                        .map_err(Into::into)
                }
                ExtensionRuntimeAction::SetSessionName { name } => {
                    application.set_session_name(&name)?;
                    Ok(Value::Null)
                }
                ExtensionRuntimeAction::SetLabel { entry_id, label } => {
                    application.set_session_label(&entry_id, label.as_deref())?;
                    Ok(Value::Null)
                }
                ExtensionRuntimeAction::GetActiveTools => {
                    serde_json::to_value(application.get_active_tool_names()).map_err(Into::into)
                }
                ExtensionRuntimeAction::GetAllTools => {
                    let tools = application
                        .get_all_tools()
                        .into_iter()
                        .map(|tool| tool.name)
                        .collect::<Vec<_>>();
                    serde_json::to_value(tools).map_err(Into::into)
                }
                ExtensionRuntimeAction::SetActiveTools { tool_names } => {
                    application.set_active_tools_by_name(&tool_names).await?;
                    Ok(Value::Null)
                }
                ExtensionRuntimeAction::GetCommands => {
                    serde_json::to_value(application.commands_catalog()).map_err(Into::into)
                }
                ExtensionRuntimeAction::SetModel { model } => {
                    let canonical = pi_ai::get_model(&model.provider, &model.id)
                        .ok_or_else(|| anyhow!(
                            "extension model {}/{} is not registered",
                            model.provider,
                            model.id
                        ))?;
                    application
                        .inner
                        .session
                        .set_model_with_resolved_auth(canonical)
                        .await?;
                    Ok(Value::Bool(true))
                }
                ExtensionRuntimeAction::SetThinkingLevel { level } => {
                    application.set_thinking_level(level);
                    Ok(Value::Null)
                }
                ExtensionRuntimeAction::Abort => {
                    application.abort().await;
                    Ok(Value::Null)
                }
                ExtensionRuntimeAction::Shutdown => {
                    application.cleanup().await;
                    Ok(Value::Null)
                }
                ExtensionRuntimeAction::Compact { custom_instructions } => {
                    serde_json::to_value(application.compact(custom_instructions.as_deref()).await?)
                        .map_err(Into::into)
                }
                ExtensionRuntimeAction::Reload => {
                    serde_json::to_value(application.reload().await?).map_err(Into::into)
                }
                ExtensionRuntimeAction::WaitForIdle => {
                    application.wait_for_idle().await;
                    Ok(Value::Null)
                }
            }
        })
    }
}

const fn message_delivery(delivery: ExtensionMessageDelivery) -> MessageDelivery {
    match delivery {
        ExtensionMessageDelivery::Steer => MessageDelivery::Steer,
        ExtensionMessageDelivery::FollowUp => MessageDelivery::FollowUp,
        ExtensionMessageDelivery::NextTurn => MessageDelivery::NextTurn,
    }
}


impl ApplicationInner {
    fn publish(&self, event: ApplicationEvent) {
        let _ = self.events.send(event);
    }
}
impl Drop for ApplicationInner {
    fn drop(&mut self) {
        self.process_manager.shutdown_owner_now(&self.process_owner_id);
        if let Some(task) = self.process_events.get_mut().take() {
            task.abort();
        }
        if let Some(task) = self.session_events.get_mut().take() {
            task.abort();
        }
        self.session_subscription.get_mut().take();

        let session = self.session.clone();
        let active_run = self.active_run.get_mut().take();
        let loop_scheduler = self.loop_scheduler.get_mut().take();
        let orchestration_runtime = self.orchestration_runtime.get_mut().take();
        let extension_runtime = self
            .extension_runtime
            .get_mut()
            .take()
            .map(|(runtime, _)| runtime);
        let cleanup = async move {
            session.abort().await;
            if let Some(run) = active_run {
                let _ = run.await;
            } else {
                session.wait_for_idle().await;
            }
            if let Some(runtime) = loop_scheduler {
                runtime.shutdown().await;
            }
            if let Some(runtime) = orchestration_runtime {
                runtime.shutdown().await;
            }
            session.abort().await;
            session.wait_for_idle().await;
            if let Some(runtime) = extension_runtime {
                runtime.shutdown_with_reason("drop").await;
            }
        };
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(cleanup);
        } else {
            std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("building application cleanup runtime");
                runtime.block_on(cleanup);
            });
        }
    }
}


async fn run_loop_turn(
    inner: std::sync::Weak<ApplicationInner>,
    request: crate::LoopRunRequest,
    cancel: tokio_util::sync::CancellationToken,
) -> std::result::Result<(), String> {
    let Some(inner) = inner.upgrade() else {
        return Err("application stopped".to_owned());
    };
    request.report(crate::LoopRunState::Queued);
    let turn_guard = tokio::select! {
        _ = cancel.cancelled() => return Err("loop cancelled".to_owned()),
        guard = inner.turn_gate.clone().lock_owned() => guard,
    };
    if cancel.is_cancelled() {
        return Err("loop cancelled".to_owned());
    }
    request.report(crate::LoopRunState::Started);
    inner.loop_turn_active.store(true, Ordering::Release);
    let session = inner.session.clone();
    let run = session.run(&request.prompt, Vec::new());
    tokio::pin!(run);
    let result = tokio::select! {
        result = &mut run => result.map(|_| ()).map_err(|error| error.to_string()),
        _ = cancel.cancelled() => {
            session.abort().await;
            match run.await {
                Ok(_) => Err("loop cancelled".to_owned()),
                Err(error) => Err(error.to_string()),
            }
        }
    };
    inner.loop_turn_active.store(false, Ordering::Release);
    inner.publish(ApplicationEvent::AgentSettled);
    drop(turn_guard);
    result
}

fn process_event_owner(event: &ProcessEvent) -> &ProcessOwnerId {
    match event {
        ProcessEvent::ProcessStarted { process } | ProcessEvent::ProcessExited { process } => {
            &process.owner_id
        }
        ProcessEvent::ProcessOutput { owner_id, .. } => owner_id,
    }
}

fn session_extension_event(event: &SessionEvent) -> Option<ExtensionEvent> {
    let (name, data) = match event {
        SessionEvent::CompactionStart { reason } => (
            "session_before_compact",
            serde_json::json!({
                "reason": reason,
                "willRetry": matches!(reason, CompactionReason::Overflow),
            }),
        ),
        SessionEvent::CompactionEnd {
            reason,
            result: Some(result),
            aborted: false,
            will_retry,
            ..
        } => (
            "session_compact",
            serde_json::json!({
                "result": result,
                "fromExtension": false,
                "reason": reason,
                "willRetry": will_retry,
            }),
        ),
        _ => return None,
    };
    Some(ExtensionEvent::new(name, data))
}

fn agent_extension_event(event: &AgentEvent) -> Option<ExtensionEvent> {
    let data = serde_json::to_value(event).ok()?;
    let name = data.get("type")?.as_str()?.to_owned();
    Some(ExtensionEvent::new(name, data))
}

fn extension_load_report_error(
    operation: &str,
    report: &crate::ExtensionLoadReport,
) -> anyhow::Error {
    anyhow!(
        "{operation} rejected {} extension(s): {}",
        report.failures.len(),
        report
            .failures
            .iter()
            .map(|failure| format!(
                "{} ({}): {}",
                failure.extension_id,
                failure.path.display(),
                failure.message
            ))
            .collect::<Vec<_>>()
            .join("; ")
    )
}

fn prompt_text(messages: &[Message]) -> String {
    messages
        .iter()
        .find_map(|message| match message {
            Message::User(user) => Some(
                user.content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<String>(),
            ),
            _ => None,
        })
        .unwrap_or_default()
}

fn prompt_images(messages: &[Message]) -> Vec<ContentBlock> {
    messages
        .iter()
        .find_map(|message| match message {
            Message::User(user) => Some(
                user.content
                    .iter()
                    .filter(|block| matches!(block, ContentBlock::Image { .. }))
                    .cloned()
                    .collect(),
            ),
            _ => None,
        })
        .unwrap_or_default()
}

fn extension_custom_message(message: crate::ExtensionCustomMessage) -> Message {
    Message::Custom(CustomMessage {
        custom_type: message.custom_type,
        content: message.content,
        display: message.display,
        details: message.details,
        timestamp: pi_ai::now_millis(),
    })
}

fn extension_usage(usage: crate::ExtensionUsage) -> pi_ai::Usage {
    pi_ai::Usage {
        input: usage.input,
        output: usage.output,
        cache_read: usage.cache_read,
        cache_write: usage.cache_write,
        cache_write_1h: usage.cache_write_1h,
        reasoning: usage.reasoning,
        total_tokens: usage.total_tokens,
        ..pi_ai::Usage::default()
    }
}

const fn streaming_behavior_name(behavior: StreamingBehavior) -> &'static str {
    match behavior {
        StreamingBehavior::Steer => "steer",
        StreamingBehavior::FollowUp => "followUp",
    }
}


fn user_message(text: String, images: Vec<ContentBlock>) -> Message {
    let mut content = Vec::with_capacity(images.len() + usize::from(!text.is_empty()));
    if !text.is_empty() {
        content.push(ContentBlock::text(text));
    }
    content.extend(images);
    Message::User(pi_ai::UserMessage {
        content,
        timestamp: pi_ai::now_millis(),
    })
}

