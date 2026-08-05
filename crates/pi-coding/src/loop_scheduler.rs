//! Rust-native recurring prompt scheduler used by `/loop`.
//!
//! One actor owns every task, timer, pending fire, and active loop turn. It
//! never invokes a shell or nested CLI; the turn runner uses the shared
//! [`crate::Application`] runtime.

use std::{collections::VecDeque, future::{Future, pending}, path::{Path, PathBuf}, pin::Pin, sync::Arc, time::Duration};
use chrono::{DateTime, TimeDelta, Utc};
use serde::{Deserialize, Serialize};
use tokio::{sync::{mpsc, oneshot}, task::JoinHandle};
use tokio_util::sync::CancellationToken;

const MAX_LOOP_TASKS: usize = 50;
const LOOP_EXPIRY_DAYS: i64 = 7;
const LOOP_STATE_VERSION: u32 = 1;
type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParsedLoopArgs<'a> { pub interval: Option<&'a str>, pub prompt: &'a str }

/// Extract an unambiguous leading compact or bare-second interval. Natural-language
/// schedules remain in the prompt unless they begin with a bare integer.
#[must_use]
pub fn parse_loop_args(args: &str) -> ParsedLoopArgs<'_> {
    let trimmed = args.trim();
    if let Some(space) = trimmed.find(char::is_whitespace) {
        let first = &trimmed[..space];
        let rest = trimmed[space..].trim_start();
        if is_interval_token(first) && !rest.is_empty() { return ParsedLoopArgs { interval: Some(first), prompt: rest }; }
    }
    ParsedLoopArgs { interval: None, prompt: trimmed }
}

#[must_use]
pub fn is_interval_token(value: &str) -> bool {
    if value.is_empty() { return false; }
    if value.chars().all(|character| character.is_ascii_digit()) {
        return true;
    }
    if value.len() < 2 { return false; }
    let (digits, suffix) = value.split_at(value.len() - 1);
    !digits.is_empty()
        && matches!(suffix, "s" | "m" | "h" | "d")
        && digits.chars().all(|character| character.is_ascii_digit())
}

pub fn parse_loop_interval(value: &str) -> Result<u64, LoopSchedulerError> {
    let value = value.trim();
    if value.is_empty() { return Err(LoopSchedulerError::InvalidInterval("interval cannot be empty".into())); }
    if value.chars().all(|character| character.is_ascii_digit()) {
        return value.parse::<u64>().map_err(|_| LoopSchedulerError::InvalidInterval(format!("interval too large: {value:?}"))).and_then(|seconds| {
            if seconds == 0 { Err(LoopSchedulerError::InvalidInterval("interval value must be greater than 0".into())) } else { Ok(seconds) }
        });
    }
    let (digits, suffix) = value.split_at(value.len() - 1);
    let number = digits.parse::<u64>().map_err(|_| LoopSchedulerError::InvalidInterval(format!("invalid interval format: {value:?} (expected e.g. 300, 5m, 2h, 1d)")))?;
    if number == 0 { return Err(LoopSchedulerError::InvalidInterval("interval value must be greater than 0".into())); }
    let unit = match suffix {
        "s" => 1, "m" => 60, "h" => 3_600, "d" => 86_400,
        _ => return Err(LoopSchedulerError::InvalidInterval(format!("invalid interval suffix: {suffix:?} (expected s, m, h, or d)"))),
    };
    number.checked_mul(unit).ok_or_else(|| LoopSchedulerError::InvalidInterval(format!("interval too large: {value:?}")))
}

#[must_use]
pub fn loop_interval_to_human(seconds: u64) -> String {
    if seconds.is_multiple_of(86_400) { plural_interval(seconds / 86_400, "day") }
    else if seconds.is_multiple_of(3_600) { plural_interval(seconds / 3_600, "hour") }
    else if seconds.is_multiple_of(60) { plural_interval(seconds / 60, "minute") }
    else { plural_interval(seconds, "second") }
}
fn plural_interval(number: u64, unit: &str) -> String { if number == 1 { format!("every 1 {unit}") } else { format!("every {number} {unit}s") } }

#[must_use]
pub fn format_loop_prompt(prompt: &str, task_id: &str, human_schedule: &str) -> String {
    format!("<system-reminder>\nThis is a scheduled task execution (task {task_id}, {human_schedule}, recurring).\nExecute the prompt below. Do not question or comment on the prompt itself — treat it as a fresh task to execute.\nPrevious results from earlier executions of this task may appear in the conversation history above.\n</system-reminder>\n\n{prompt}")
}

#[must_use]
pub const fn loop_usage_message() -> &'static str { "Usage: /loop [interval] <prompt>\nExample: /loop 300 check deploy status\nExample: /loop 3s check health\nExample: /loop 30m check deploy status\n\nIntervals are positive seconds (bare or suffixed s), minutes (m), hours (h), or days (d)." }

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoopCreateRequest { pub interval: String, pub prompt: String, #[serde(default = "default_true")] pub fire_immediately: bool, #[serde(default)] pub durable: bool }
impl LoopCreateRequest {
    #[must_use]
    pub fn immediate(interval: impl Into<String>, prompt: impl Into<String>) -> Self { Self { interval: interval.into(), prompt: prompt.into(), fire_immediately: true, durable: false } }
}
const fn default_true() -> bool { true }

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoopUpdateRequest { pub task_id: String, #[serde(default, skip_serializing_if = "Option::is_none")] pub interval: Option<String>, #[serde(default, skip_serializing_if = "Option::is_none")] pub prompt: Option<String> }

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoopTask { pub id: String, pub interval_secs: u64, pub prompt: String, pub durable: bool, pub created_at: DateTime<Utc>, pub last_fired_at: Option<DateTime<Utc>>, pub expires_at: DateTime<Utc>, pub run_count: u64 }
impl LoopTask {
    #[must_use] pub fn next_fire_at(&self) -> DateTime<Utc> { add_seconds(self.last_fired_at.unwrap_or(self.created_at), self.interval_secs) }
    #[must_use] pub fn is_expired(&self, now: DateTime<Utc>) -> bool { now >= self.expires_at }
    #[must_use] pub fn human_schedule(&self) -> String { loop_interval_to_human(self.interval_secs) }
}

pub const LOOP_SCHEDULED_MESSAGE_TYPE: &str = "loop_scheduled_turn";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoopMessageView<'a> {
    pub task_id: &'a str,
    pub prompt: &'a str,
    pub schedule: &'a str,
}

#[must_use]
pub fn loop_message_view(message: &pi_ai::CustomMessage) -> Option<LoopMessageView<'_>> {
    if message.custom_type != LOOP_SCHEDULED_MESSAGE_TYPE {
        return None;
    }
    let details = message.details.as_ref()?;
    Some(LoopMessageView {
        task_id: details.get("taskId")?.as_str()?,
        prompt: details.get("prompt")?.as_str()?,
        schedule: details.get("schedule")?.as_str()?,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoopSkipReason { AlreadyRunning, AlreadyQueued }
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoopRemovalReason { Deleted, Cancelled, Expired, SessionChanged, Shutdown }

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LoopEvent {
    #[serde(rename = "loop_created")] Created { task: LoopTask, restored: bool },
    #[serde(rename = "loop_updated")] Updated { task: LoopTask },
    #[serde(rename = "loop_queued")] Queued { task_id: String, position: usize, next_fire_at: DateTime<Utc> },
    #[serde(rename = "loop_fired")] Fired { task_id: String, fired_at: DateTime<Utc>, next_fire_at: DateTime<Utc> },
    #[serde(rename = "loop_skipped")] Skipped { task_id: String, reason: LoopSkipReason, next_fire_at: DateTime<Utc> },
    #[serde(rename = "loop_finished")] Finished { task_id: String, finished_at: DateTime<Utc>, next_fire_at: Option<DateTime<Utc>> },
    #[serde(rename = "loop_failed")] Failed { task_id: String, message: String, next_fire_at: Option<DateTime<Utc>> },
    #[serde(rename = "loop_removed")] Removed { task_id: String, reason: LoopRemovalReason },
    #[serde(rename = "loop_scheduler_failed")] SchedulerFailed { message: String },
}

#[derive(thiserror::Error, Debug)]
pub enum LoopSchedulerError {
    #[error("invalid interval: {0}")] InvalidInterval(String),
    #[error("loop prompt cannot be empty")] EmptyPrompt,
    #[error("maximum of {0} scheduled tasks reached")] TaskLimitReached(usize),
    #[error("no scheduled loop with id {0}")] TaskNotFound(String),
    #[error("nothing to update; provide interval and/or prompt")] EmptyUpdate,
    #[error("loop scheduler is not active")] Inactive,
    #[error("loop scheduler stopped")] Stopped,
    #[error("failed to persist loop scheduler state: {0}")] Persistence(#[source] std::io::Error),
    #[error("failed to decode loop scheduler state: {0}")] Decode(#[source] serde_json::Error),
}
/// Validated target loop sidecar ready for an atomic session cutover.
///
/// Built by [`prepare_loop_activation`] before any active-actor mutation so
/// target decode failures cannot strand the current session mid-switch.
#[derive(Clone, Debug)]
pub struct PreparedLoopActivation {
    storage_path: Option<PathBuf>,
    tasks: Vec<LoopTask>,
    events: Vec<LoopEvent>,
}

pub struct PreparedLoopSessionSwitch {
    activation: PreparedLoopActivation,
}

/// Read and decode the target session's loop sidecar without touching the actor.
///
/// Missing sidecars yield an empty task list. Malformed or unsupported versions
/// return [`LoopSchedulerError::Decode`] / persistence errors unchanged.
pub(crate) async fn prepare_loop_activation(
    session_file: Option<&Path>,
) -> Result<PreparedLoopActivation, LoopSchedulerError> {
    prepare_loop_activation_at(session_file, Utc::now()).await
}

/// Clock-aware core of [`prepare_loop_activation`].
///
/// Expiry partitioning is driven by the supplied `now` so the result is
/// consistent with whatever clock the loop actor will install under. Production
/// callers pass `Utc::now()` via the wrapper; tests pinned to a [`ManualClock`]
/// pass that clock's `now` so a target seeded with a fixed expiry is not
/// classified as expired by the real wall clock.
async fn prepare_loop_activation_at(
    session_file: Option<&Path>,
    now: DateTime<Utc>,
) -> Result<PreparedLoopActivation, LoopSchedulerError> {
    let storage_path = session_file.map(loop_state_path);
    let state = match &storage_path {
        Some(path) => load_loop_state(path).await?,
        None => PersistedLoopState {
            version: LOOP_STATE_VERSION,
            tasks: Vec::new(),
        },
    };
    let (expired_tasks, tasks): (Vec<_>, Vec<_>) =
        state.tasks.into_iter().partition(|task| task.is_expired(now));
    let mut events = expired_tasks
        .into_iter()
        .map(|task| LoopEvent::Removed {
            task_id: task.id,
            reason: LoopRemovalReason::Expired,
        })
        .collect::<Vec<_>>();
    events.extend(tasks.iter().cloned().map(|task| LoopEvent::Created {
        task,
        restored: true,
    }));
    Ok(PreparedLoopActivation {
        storage_path,
        tasks,
        events,
    })
}

pub(crate) fn prepare_loop_session_switch(
    activation: PreparedLoopActivation,
) -> PreparedLoopSessionSwitch {
    PreparedLoopSessionSwitch { activation }
}

#[derive(Clone)]
pub struct LoopSchedulerHandle {
    commands: mpsc::UnboundedSender<LoopCommand>,
}
impl LoopSchedulerHandle {
    pub async fn create(&self, request: LoopCreateRequest) -> Result<LoopTask, LoopSchedulerError> { let (reply, response) = oneshot::channel(); self.send(LoopCommand::Create { request, reply })?; receive(response).await }
    pub async fn update(&self, request: LoopUpdateRequest) -> Result<LoopTask, LoopSchedulerError> { let (reply, response) = oneshot::channel(); self.send(LoopCommand::Update { request, reply })?; receive(response).await }
    pub async fn list(&self) -> Result<Vec<LoopTask>, LoopSchedulerError> { let (reply, response) = oneshot::channel(); self.send(LoopCommand::List { reply })?; receive(response).await }
    pub async fn delete(&self, task_id: &str) -> Result<bool, LoopSchedulerError> { self.delete_inner(task_id, false).await }
    pub async fn cancel(&self, task_id: &str) -> Result<bool, LoopSchedulerError> { self.delete_inner(task_id, true).await }
    async fn delete_inner(&self, task_id: &str, cancel_active: bool) -> Result<bool, LoopSchedulerError> { let (reply, response) = oneshot::channel(); self.send(LoopCommand::Delete { task_id: task_id.to_owned(), cancel_active, reply })?; receive(response).await }
    /// Cancel the active iteration in-flight without removing the loop task.
    /// Returns `true` when an active run was cancelled, `false` when none was
    /// running. Used by `Application::abort` so a user Esc during a loop-owned
    /// turn settles as a cancellation (`RunResult::Cancelled`, silent — the loop
    /// remains scheduled) instead of a failure (`LoopEvent::Failed` toast).
    pub async fn cancel_active_iteration(&self) -> Result<bool, LoopSchedulerError> { let (reply, response) = oneshot::channel(); self.send(LoopCommand::CancelActive { reply })?; receive(response).await }
    pub(crate) async fn suspend(&self, reason: LoopRemovalReason) -> Result<(), LoopSchedulerError> { let (reply, response) = oneshot::channel(); self.send(LoopCommand::Suspend { reason, reply })?; receive(response).await }
    pub(crate) async fn activate(&self, session_file: Option<PathBuf>) -> Result<(), LoopSchedulerError> { let (reply, response) = oneshot::channel(); self.send(LoopCommand::Activate { storage_path: session_file.as_deref().map(loop_state_path), reply })?; receive(response).await }
    /// Atomically cut over to a previously prepared target session sidecar.
    ///
    /// Persists the currently active durable state first. On persistence failure the
    /// actor is left untouched. After that commit point the old active/queued work is
    /// cancelled, removal events are emitted, and the prepared target tasks are installed
    /// with no further fallible I/O.
    pub(crate) async fn commit_session_switch(
        &self,
        prepared: PreparedLoopActivation,
        reason: LoopRemovalReason,
    ) -> Result<(), LoopSchedulerError> {
        let (reply, response) = oneshot::channel();
        self.send(LoopCommand::SwitchSession { prepared, reason, reply })?;
        receive(response).await
    }
    /// Persist and quiesce the current session before any external live-state
    /// mutation. The target is already decoded; after this succeeds, activation
    /// is an infallible actor message and in-memory install.
    pub(crate) async fn prepare_session_switch(
        &self,
        prepared: PreparedLoopSessionSwitch,
        reason: LoopRemovalReason,
    ) -> Result<PreparedLoopActivation, LoopSchedulerError> {
        self.suspend(reason).await?;
        Ok(prepared.activation)
    }

    pub(crate) fn restore_prepared_session(&self, prepared: PreparedLoopActivation) {
        self.commit_prepared_session_switch(prepared);
    }
    pub(crate) fn commit_prepared_session_switch(&self, prepared: PreparedLoopActivation) {
        self.commands
            .send(LoopCommand::InstallPrepared { prepared })
            .expect("prepared loop scheduler remains live through session cutover");
    }
    fn send(&self, command: LoopCommand) -> Result<(), LoopSchedulerError> { self.commands.send(command).map_err(|_| LoopSchedulerError::Stopped) }
}
async fn receive<T>(response: oneshot::Receiver<Result<T, LoopSchedulerError>>) -> Result<T, LoopSchedulerError> { response.await.map_err(|_| LoopSchedulerError::Stopped)? }

pub(crate) struct LoopRunRequest {
    pub task_id: String,
    pub prompt: String,
    pub model_prompt: String,
    pub human_schedule: String,
    pub state: Option<LoopRunStateSink>,
}

impl LoopRunRequest {
    pub fn report(&self, state: LoopRunState) {
        if let Some(sink) = &self.state {
            sink(state);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LoopRunState {
    Queued,
    Started,
}

pub(crate) type LoopRunStateSink = Arc<dyn Fn(LoopRunState) + Send + Sync>;
pub(crate) type LoopTurnRunner = Arc<dyn Fn(LoopRunRequest, CancellationToken) -> BoxFuture<Result<(), String>> + Send + Sync>;
pub(crate) type LoopEventSink = Arc<dyn Fn(LoopEvent) + Send + Sync>;
pub(crate) struct LoopSchedulerRuntime { pub handle: LoopSchedulerHandle, cancel: CancellationToken, join: JoinHandle<()> }
impl LoopSchedulerRuntime { pub async fn shutdown(self) { self.cancel.cancel(); let _ = self.join.await; } }

pub(crate) fn start_loop_scheduler(session_file: Option<&Path>, runner: LoopTurnRunner, events: LoopEventSink) -> LoopSchedulerRuntime { start_loop_scheduler_with_clock(session_file.map(loop_state_path), runner, events, Arc::new(TokioLoopClock)) }
fn start_loop_scheduler_with_clock(
    storage_path: Option<PathBuf>,
    runner: LoopTurnRunner,
    events: LoopEventSink,
    clock: Arc<dyn LoopClock>,
) -> LoopSchedulerRuntime {
    start_loop_scheduler_with_clock_and_persist_hook(storage_path, runner, events, clock, None)
}

fn start_loop_scheduler_with_clock_and_persist_hook(
    storage_path: Option<PathBuf>,
    runner: LoopTurnRunner,
    events: LoopEventSink,
    clock: Arc<dyn LoopClock>,
    persist_fail: Option<Arc<std::sync::atomic::AtomicBool>>,
) -> LoopSchedulerRuntime {
    let (commands, command_rx) = mpsc::unbounded_channel();
    let (completions, completion_rx) = mpsc::unbounded_channel();
    let cancel = CancellationToken::new();
    let actor = LoopSchedulerActor {
        tasks: Vec::new(),
        queue: VecDeque::new(),
        active: None,
        command_rx,
        completions,
        completion_rx,
        runner,
        events,
        clock,
        cancel: cancel.clone(),
        storage_path,
        active_session: true,
        session_generation: 0,
        pending_suspend: None,
        pending_switch: None,
        persist_fail,
    };
    let join = tokio::spawn(actor.run());
    LoopSchedulerRuntime {
        handle: LoopSchedulerHandle { commands },
        cancel,
        join,
    }
}
enum LoopCommand {
    Create { request: LoopCreateRequest, reply: oneshot::Sender<Result<LoopTask, LoopSchedulerError>> }, Update { request: LoopUpdateRequest, reply: oneshot::Sender<Result<LoopTask, LoopSchedulerError>> }, List { reply: oneshot::Sender<Result<Vec<LoopTask>, LoopSchedulerError>> }, Delete { task_id: String, cancel_active: bool, reply: oneshot::Sender<Result<bool, LoopSchedulerError>> }, CancelActive { reply: oneshot::Sender<Result<bool, LoopSchedulerError>> }, Suspend { reason: LoopRemovalReason, reply: oneshot::Sender<Result<(), LoopSchedulerError>> }, Activate { storage_path: Option<PathBuf>, reply: oneshot::Sender<Result<(), LoopSchedulerError>> }, SwitchSession { prepared: PreparedLoopActivation, reason: LoopRemovalReason, reply: oneshot::Sender<Result<(), LoopSchedulerError>> }, InstallPrepared { prepared: PreparedLoopActivation },
}
struct PendingRun { request: LoopRunRequest, next_fire_at: DateTime<Utc> }
struct ActiveRun { task_id: String, cancel: CancellationToken, join: JoinHandle<()>, generation: u64, report_completion: bool }
struct RunCompletion { task_id: String, generation: u64, result: RunResult }
enum RunResult { Finished, Failed(String), Cancelled }
struct PendingSuspend { reply: oneshot::Sender<Result<(), LoopSchedulerError>> }
struct PendingSwitch {
    prepared: PreparedLoopActivation,
    reply: oneshot::Sender<Result<(), LoopSchedulerError>>,
}
struct LoopSchedulerActor {
    tasks: Vec<LoopTask>,
    queue: VecDeque<PendingRun>,
    active: Option<ActiveRun>,
    command_rx: mpsc::UnboundedReceiver<LoopCommand>,
    completions: mpsc::UnboundedSender<RunCompletion>,
    completion_rx: mpsc::UnboundedReceiver<RunCompletion>,
    runner: LoopTurnRunner,
    events: LoopEventSink,
    clock: Arc<dyn LoopClock>,
    cancel: CancellationToken,
    storage_path: Option<PathBuf>,
    active_session: bool,
    session_generation: u64,
    pending_suspend: Option<PendingSuspend>,
    pending_switch: Option<PendingSwitch>,
    /// Test-only: when set and true, `persist` fails before any disk I/O.
    persist_fail: Option<Arc<std::sync::atomic::AtomicBool>>,
}

impl LoopSchedulerActor {
    async fn run(mut self) {
        if let Err(error) = self.load_and_announce().await { self.emit_scheduler_error(error); }
        loop { let deadline = self.next_deadline(); tokio::select! { biased; _ = self.cancel.cancelled() => break, completion = self.completion_rx.recv() => if let Some(completion) = completion { self.finish_run(completion); }, command = self.command_rx.recv() => { let Some(command) = command else { break }; self.handle_command(command).await; }, () = sleep_for_deadline(self.clock.clone(), deadline), if self.active_session => self.fire_one_due().await, } }
        self.shutdown().await;
    }
    fn next_deadline(&self) -> Option<DateTime<Utc>> {
        self.tasks
            .iter()
            .flat_map(|task| [task.next_fire_at(), task.expires_at])
            .min()
    }
    async fn handle_command(&mut self, command: LoopCommand) {
        match command {
            LoopCommand::Create { request, reply } => {
                let result = self.create(request).await;
                let _ = reply.send(result);
            }
            LoopCommand::Update { request, reply } => {
                let result = self.update(request).await;
                let _ = reply.send(result);
            }
            LoopCommand::List { reply } => {
                let result = if self.active_session {
                    Ok(self.tasks.clone())
                } else {
                    Err(LoopSchedulerError::Inactive)
                };
                let _ = reply.send(result);
            }
            LoopCommand::Delete {
                task_id,
                cancel_active,
                reply,
            } => {
                let result = self.delete(&task_id, cancel_active).await;
                let _ = reply.send(result);
            }
            LoopCommand::CancelActive { reply } => {
                let result = self.cancel_active_iteration();
                let _ = reply.send(result);
            }
            LoopCommand::Suspend { reason, reply } => self.suspend(reason, reply).await,
            LoopCommand::Activate { storage_path, reply } => {
                let result = self.activate(storage_path).await;
                let _ = reply.send(result);
            }
            LoopCommand::SwitchSession {
                prepared,
                reason,
                reply,
            } => {
                self.switch_session(prepared, reason, reply).await;
            }
            LoopCommand::InstallPrepared { prepared } => self.install_prepared(prepared),
        }
    }
    async fn create(&mut self, request: LoopCreateRequest) -> Result<LoopTask, LoopSchedulerError> {
        self.require_active()?; if self.tasks.len() >= MAX_LOOP_TASKS { return Err(LoopSchedulerError::TaskLimitReached(MAX_LOOP_TASKS)); } let prompt = request.prompt.trim(); if prompt.is_empty() { return Err(LoopSchedulerError::EmptyPrompt); } let interval_secs = parse_loop_interval(&request.interval)?; let now = self.clock.now();
        let id = uuid::Uuid::now_v7().simple().to_string();
        let task = LoopTask { id: id[id.len() - 12..].to_owned(), interval_secs, prompt: prompt.to_owned(), durable: request.durable, created_at: if request.fire_immediately { subtract_seconds(now, interval_secs) } else { now }, last_fired_at: None, expires_at: now + TimeDelta::days(LOOP_EXPIRY_DAYS), run_count: 0 };
        self.tasks.push(task.clone()); if let Err(error) = self.persist().await { self.tasks.pop(); return Err(error); } self.emit(LoopEvent::Created { task: task.clone(), restored: false }); Ok(task)
    }
    async fn update(&mut self, request: LoopUpdateRequest) -> Result<LoopTask, LoopSchedulerError> {
        self.require_active()?; if request.interval.is_none() && request.prompt.is_none() { return Err(LoopSchedulerError::EmptyUpdate); } let interval_secs = request.interval.as_deref().map(parse_loop_interval).transpose()?; let prompt = request.prompt.as_deref().map(str::trim).map(|value| if value.is_empty() { Err(LoopSchedulerError::EmptyPrompt) } else { Ok(value.to_owned()) }).transpose()?; let index = self.task_index(&request.task_id).ok_or_else(|| LoopSchedulerError::TaskNotFound(request.task_id.clone()))?; let previous = self.tasks[index].clone(); if let Some(prompt) = prompt { self.tasks[index].prompt = prompt; } if let Some(interval_secs) = interval_secs { self.tasks[index].interval_secs = interval_secs; if self.tasks[index].next_fire_at() <= self.clock.now() { self.tasks[index].last_fired_at = Some(self.clock.now()); } } let updated = self.tasks[index].clone(); if let Err(error) = self.persist().await { self.tasks[index] = previous; return Err(error); } self.emit(LoopEvent::Updated { task: updated.clone() }); Ok(updated)
    }
    async fn delete(&mut self, task_id: &str, cancel_active: bool) -> Result<bool, LoopSchedulerError> {
        self.require_active()?; let Some(index) = self.task_index(task_id) else { return Ok(false); }; let removed = self.tasks.remove(index); if let Err(error) = self.persist().await { self.tasks.insert(index, removed); return Err(error); } self.queue.retain(|pending| pending.request.task_id != task_id); if let Some(active) = self.active.as_mut().filter(|active| active.task_id == task_id) { active.report_completion = false; if cancel_active { active.cancel.cancel(); } } self.emit(LoopEvent::Removed { task_id: task_id.to_owned(), reason: if cancel_active { LoopRemovalReason::Cancelled } else { LoopRemovalReason::Deleted } }); Ok(true)
    }
    /// Cancel the active iteration's token without removing the task. The loop
    /// remains scheduled and its next fire proceeds normally; the cancelled
    /// iteration settles as `RunResult::Cancelled` (no `LoopEvent::Failed`).
    /// This distinguishes an explicit user abort from a real provider/runtime
    /// failure, which completes without the token cancelled.
    fn cancel_active_iteration(&self) -> Result<bool, LoopSchedulerError> {
        if let Some(active) = self.active.as_ref() {
            active.cancel.cancel();
            Ok(true)
        } else {
            Ok(false)
        }
    }
    async fn fire_one_due(&mut self) {
        let now = self.clock.now();
        let Some(index) = self
            .tasks
            .iter()
            .position(|task| task.is_expired(now) || task.next_fire_at() <= now)
        else {
            return;
        };
        if self.tasks[index].is_expired(now) {
            let task = self.tasks.remove(index);
            if let Err(error) = self.persist().await {
                self.tasks.insert(index, task);
                self.emit_scheduler_error(error);
                return;
            }
            self.emit(LoopEvent::Removed {
                task_id: task.id,
                reason: LoopRemovalReason::Expired,
            });
            return;
        }

        let previous = self.tasks[index].clone();
        self.tasks[index].last_fired_at = Some(now);
        self.tasks[index].run_count = self.tasks[index].run_count.saturating_add(1);
        let task = self.tasks[index].clone();
        let next_fire_at = task.next_fire_at();
        if let Err(error) = self.persist().await {
            self.tasks[index] = previous;
            self.emit_scheduler_error(error);
            return;
        }

        if self.active.as_ref().is_some_and(|active| active.task_id == task.id) {
            self.emit(LoopEvent::Skipped {
                task_id: task.id,
                reason: LoopSkipReason::AlreadyRunning,
                next_fire_at,
            });
            return;
        }
        if self.queue.iter().any(|pending| pending.request.task_id == task.id) {
            self.emit(LoopEvent::Skipped {
                task_id: task.id,
                reason: LoopSkipReason::AlreadyQueued,
                next_fire_at,
            });
            return;
        }

        let human_schedule = task.human_schedule();
        let request = LoopRunRequest {
            task_id: task.id.clone(),
            prompt: task.prompt.clone(),
            model_prompt: format_loop_prompt(&task.prompt, &task.id, &human_schedule),
            human_schedule,
            state: None,
        };
        if self.active.is_some() {
            self.queue.push_back(PendingRun { request, next_fire_at });
            self.emit(LoopEvent::Queued {
                task_id: task.id,
                position: self.queue.len(),
                next_fire_at,
            });
        } else {
            self.start_run(request, next_fire_at);
        }
    }
    fn start_run(&mut self, mut request: LoopRunRequest, next_fire_at: DateTime<Utc>) {
        let task_id = request.task_id.clone();
        let event_task_id = task_id.clone();
        let events = self.events.clone();
        let clock = self.clock.clone();
        request.state = Some(Arc::new(move |state| match state {
            LoopRunState::Queued => events(LoopEvent::Queued {
                task_id: event_task_id.clone(),
                position: 1,
                next_fire_at,
            }),
            LoopRunState::Started => events(LoopEvent::Fired {
                task_id: event_task_id.clone(),
                fired_at: clock.now(),
                next_fire_at,
            }),
        }));
        let cancel = CancellationToken::new();
        let runner_cancel = cancel.clone();
        let runner = self.runner.clone();
        let completions = self.completions.clone();
        let generation = self.session_generation;
        let completion_task_id = task_id.clone();
        let join = tokio::spawn(async move {
            let result = runner(request, runner_cancel.clone()).await;
            let result = if runner_cancel.is_cancelled() {
                RunResult::Cancelled
            } else {
                result.map_or_else(RunResult::Failed, |()| RunResult::Finished)
            };
            let _ = completions.send(RunCompletion {
                task_id: completion_task_id,
                generation,
                result,
            });
        });
        self.active = Some(ActiveRun {
            task_id,
            cancel,
            join,
            generation,
            report_completion: true,
        });
    }
    fn finish_run(&mut self, completion: RunCompletion) {
        let Some(active) = self.active.take() else { return; };
        if active.generation != completion.generation || active.task_id != completion.task_id {
            return;
        }
        if active.report_completion && completion.generation == self.session_generation {
            let next_fire_at = self.tasks.iter().find(|task| task.id == completion.task_id).map(LoopTask::next_fire_at);
            match completion.result {
                RunResult::Finished => self.emit(LoopEvent::Finished { task_id: completion.task_id, finished_at: self.clock.now(), next_fire_at }),
                RunResult::Failed(message) => self.emit(LoopEvent::Failed { task_id: completion.task_id, message, next_fire_at }),
                RunResult::Cancelled => {}
            }
        }
        drop(active.join);
        if let Some(pending) = self.queue.pop_front().filter(|_| self.active_session) {
            self.start_run(pending.request, pending.next_fire_at);
        }
        self.complete_pending_suspend();
        self.complete_pending_switch();
    }
    async fn suspend(
        &mut self,
        reason: LoopRemovalReason,
        reply: oneshot::Sender<Result<(), LoopSchedulerError>>,
    ) {
        if !self.active_session || self.pending_switch.is_some() {
            let _ = reply.send(if self.active_session {
                Err(LoopSchedulerError::Inactive)
            } else {
                Ok(())
            });
            return;
        }
        if let Err(error) = self.persist().await {
            let _ = reply.send(Err(error));
            return;
        }
        self.active_session = false;
        self.session_generation = self.session_generation.wrapping_add(1);
        self.queue.clear();
        if let Some(active) = self.active.as_mut() {
            active.report_completion = false;
            active.cancel.cancel();
        }
        let tasks = std::mem::take(&mut self.tasks);
        for task in tasks {
            self.emit(LoopEvent::Removed {
                task_id: task.id,
                reason,
            });
        }
        if self.active.is_some() {
            self.pending_suspend = Some(PendingSuspend { reply });
        } else {
            let _ = reply.send(Ok(()));
        }
    }
    async fn activate(
        &mut self,
        storage_path: Option<PathBuf>,
    ) -> Result<(), LoopSchedulerError> {
        if self.active_session
            || self.active.is_some()
            || self.pending_suspend.is_some()
            || self.pending_switch.is_some()
        {
            return Err(LoopSchedulerError::Inactive);
        }
        self.storage_path = storage_path;
        self.active_session = true;
        if let Err(error) = self.load_and_announce().await {
            self.active_session = false;
            return Err(error);
        }
        Ok(())
    }
    async fn switch_session(
        &mut self,
        prepared: PreparedLoopActivation,
        reason: LoopRemovalReason,
        reply: oneshot::Sender<Result<(), LoopSchedulerError>>,
    ) {
        if !self.active_session || self.pending_suspend.is_some() || self.pending_switch.is_some() {
            let _ = reply.send(Err(LoopSchedulerError::Inactive));
            return;
        }
        // Commit gate: persist the live session first. Failure leaves the actor untouched.
        if let Err(error) = self.persist().await {
            let _ = reply.send(Err(error));
            return;
        }
        // Point of no return — only infallible in-memory cutover remains.
        self.active_session = false;
        self.session_generation = self.session_generation.wrapping_add(1);
        self.queue.clear();
        if let Some(active) = self.active.as_mut() {
            active.report_completion = false;
            active.cancel.cancel();
        }
        let tasks = std::mem::take(&mut self.tasks);
        for task in tasks {
            self.emit(LoopEvent::Removed {
                task_id: task.id,
                reason,
            });
        }
        if self.active.is_some() {
            self.pending_switch = Some(PendingSwitch { prepared, reply });
        } else {
            self.install_prepared(prepared);
            let _ = reply.send(Ok(()));
        }
    }
    fn complete_pending_suspend(&mut self) {
        if self.active.is_none()
            && let Some(pending) = self.pending_suspend.take()
        {
            let _ = pending.reply.send(Ok(()));
        }
    }
    fn complete_pending_switch(&mut self) {
        if self.active.is_none()
            && let Some(pending) = self.pending_switch.take()
        {
            self.install_prepared(pending.prepared);
            let _ = pending.reply.send(Ok(()));
        }
    }
    /// Install a prepared target without any fallible I/O or allocation.
    fn install_prepared(&mut self, prepared: PreparedLoopActivation) {
        self.storage_path = prepared.storage_path;
        self.tasks = prepared.tasks;
        for event in prepared.events {
            self.emit(event);
        }
        self.active_session = true;
    }
    async fn load_and_announce(&mut self) -> Result<(), LoopSchedulerError> {
        let Some(path) = &self.storage_path else {
            return Ok(());
        };
        let state = load_loop_state(path).await?;
        let now = self.clock.now();
        let mut expired = Vec::new();
        self.tasks = state
            .tasks
            .into_iter()
            .filter(|task| {
                if task.is_expired(now) {
                    expired.push(task.id.clone());
                    false
                } else {
                    true
                }
            })
            .collect();
        if !expired.is_empty() {
            self.persist().await?;
            for task_id in expired {
                self.emit(LoopEvent::Removed {
                    task_id,
                    reason: LoopRemovalReason::Expired,
                });
            }
        }
        for task in self.tasks.clone() {
            self.emit(LoopEvent::Created {
                task,
                restored: true,
            });
        }
        Ok(())
    }
    async fn persist(&self) -> Result<(), LoopSchedulerError> {
        if self
            .persist_fail
            .as_ref()
            .is_some_and(|flag| flag.load(std::sync::atomic::Ordering::SeqCst))
        {
            return Err(LoopSchedulerError::Persistence(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "loop persist failpoint",
            )));
        }
        let Some(path) = &self.storage_path else {
            return Ok(());
        };
        save_loop_state(
            path,
            &PersistedLoopState {
                version: LOOP_STATE_VERSION,
                tasks: self
                    .tasks
                    .iter()
                    .filter(|task| task.durable)
                    .cloned()
                    .collect(),
            },
        )
        .await
    }
    async fn shutdown(&mut self) {
        if let Err(error) = self.persist().await {
            self.emit_scheduler_error(error);
        }
        self.queue.clear();
        if let Some(active) = self.active.take() {
            active.cancel.cancel();
            let _ = active.join.await;
        }
        let tasks = std::mem::take(&mut self.tasks);
        for task in tasks {
            self.emit(LoopEvent::Removed {
                task_id: task.id,
                reason: LoopRemovalReason::Shutdown,
            });
        }
        if let Some(pending) = self.pending_suspend.take() {
            let _ = pending.reply.send(Ok(()));
        }
        if let Some(pending) = self.pending_switch.take() {
            // Shutdown aborted an in-flight cutover after old state was already
            // cancelled; the prepared target was never installed.
            let _ = pending.reply.send(Err(LoopSchedulerError::Stopped));
        }
    }
    fn require_active(&self) -> Result<(), LoopSchedulerError> { if self.active_session { Ok(()) } else { Err(LoopSchedulerError::Inactive) } }
    fn task_index(&self, task_id: &str) -> Option<usize> { self.tasks.iter().position(|task| task.id == task_id) }
    fn emit(&self, event: LoopEvent) { (self.events)(event); }
    fn emit_scheduler_error(&self, error: LoopSchedulerError) { self.emit(LoopEvent::SchedulerFailed { message: error.to_string() }); }
}

trait LoopClock: Send + Sync { fn now(&self) -> DateTime<Utc>; fn sleep_until(&self, deadline: DateTime<Utc>) -> BoxFuture<()>; }
struct TokioLoopClock;
impl LoopClock for TokioLoopClock { fn now(&self) -> DateTime<Utc> { Utc::now() } fn sleep_until(&self, deadline: DateTime<Utc>) -> BoxFuture<()> { let delay = (deadline - Utc::now()).to_std().unwrap_or(Duration::ZERO); Box::pin(tokio::time::sleep(delay)) } }
async fn sleep_for_deadline(clock: Arc<dyn LoopClock>, deadline: Option<DateTime<Utc>>) { if let Some(deadline) = deadline { clock.sleep_until(deadline).await; } else { pending::<()>().await; } }
fn add_seconds(time: DateTime<Utc>, seconds: u64) -> DateTime<Utc> { i64::try_from(seconds).ok().and_then(TimeDelta::try_seconds).and_then(|delta| time.checked_add_signed(delta)).unwrap_or(DateTime::<Utc>::MAX_UTC) }
fn subtract_seconds(time: DateTime<Utc>, seconds: u64) -> DateTime<Utc> { i64::try_from(seconds).ok().and_then(TimeDelta::try_seconds).and_then(|delta| time.checked_sub_signed(delta)).unwrap_or(DateTime::<Utc>::MIN_UTC) }
fn loop_state_path(session_file: &Path) -> PathBuf { let mut name = session_file.file_name().map_or_else(|| "session".into(), |name| name.to_os_string()); name.push(".loops.json"); session_file.with_file_name(name) }

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PersistedLoopState { version: u32, tasks: Vec<LoopTask> }
async fn load_loop_state(path: &Path) -> Result<PersistedLoopState, LoopSchedulerError> {
    let bytes = match tokio::fs::read(path).await { Ok(bytes) => bytes, Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(PersistedLoopState { version: LOOP_STATE_VERSION, tasks: Vec::new() }), Err(error) => return Err(LoopSchedulerError::Persistence(error)), }; let state = serde_json::from_slice::<PersistedLoopState>(&bytes).map_err(LoopSchedulerError::Decode)?; if state.version != LOOP_STATE_VERSION { return Err(LoopSchedulerError::Decode(serde_json::Error::io(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("unsupported loop state version {}; expected {LOOP_STATE_VERSION}", state.version))))); } Ok(state)
}
async fn save_loop_state(path: &Path, state: &PersistedLoopState) -> Result<(), LoopSchedulerError> {
    let bytes = serde_json::to_vec(state).map_err(LoopSchedulerError::Decode)?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    tokio::fs::create_dir_all(parent)
        .await
        .map_err(LoopSchedulerError::Persistence)?;
    let temporary = path.with_extension(format!("tmp-{}", uuid::Uuid::now_v7().simple()));
    tokio::fs::write(&temporary, bytes)
        .await
        .map_err(LoopSchedulerError::Persistence)?;
    if let Err(error) = tokio::fs::rename(&temporary, path).await {
        let _ = tokio::fs::remove_file(&temporary).await;
        return Err(LoopSchedulerError::Persistence(error));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use super::*;
    use tokio::sync::Notify;

    #[test]
    fn parser_matches_researched_fixtures() {
        let fixtures = [("5m check deploy status", Some("5m"), "check deploy status"), ("check deploy status", None, "check deploy status"), ("2h run tests", Some("2h"), "run tests"), ("1d daily report", Some("1d"), "daily report"), ("60s ping health", Some("60s"), "ping health"), ("5m", None, "5m"), ("check 5m deploy", None, "check 5m deploy"), ("", None, ""), ("   ", None, ""), ("5x do x", None, "5x do x"), ("5 do x", Some("5"), "do x"), ("m do x", None, "m do x"), ("55mm do x", None, "55mm do x"), ("0m do x", Some("0m"), "do x"), ("0s do x", Some("0s"), "do x"), ("abc do x", None, "abc do x"), ("99999999999999999999m do x", Some("99999999999999999999m"), "do x"), ("every 30 minutes do x", None, "every 30 minutes do x"), ("30 min check deploy", Some("30"), "min check deploy"), ("1 hour run report", Some("1"), "hour run report"), ("run the report every 1h", None, "run the report every 1h")];
        for (input, interval, prompt) in fixtures { assert_eq!(parse_loop_args(input), ParsedLoopArgs { interval, prompt }, "fixture {input:?}"); }
        assert_eq!(parse_loop_args("300 echo hello"), ParsedLoopArgs { interval: Some("300"), prompt: "echo hello" });
        assert_eq!(parse_loop_args("3s echo hello"), ParsedLoopArgs { interval: Some("3s"), prompt: "echo hello" });
        assert_eq!(parse_loop_args("0 echo hello"), ParsedLoopArgs { interval: Some("0"), prompt: "echo hello" });
    }
    #[test]
    fn interval_parse_preserves_seconds_rejects_invalid_and_formats() { assert_eq!(parse_loop_interval("300").expect("bare seconds"), 300); assert_eq!(parse_loop_interval("5m").expect("minutes"), 300); assert_eq!(parse_loop_interval("2h").expect("hours"), 7_200); assert_eq!(parse_loop_interval("1d").expect("days"), 86_400); assert_eq!(parse_loop_interval("1s").expect("seconds"), 1); assert_eq!(parse_loop_interval("3s").expect("seconds"), 3); assert!(parse_loop_interval("0").is_err()); assert!(parse_loop_interval("0m").is_err()); assert!(parse_loop_interval("18446744073709551615d").is_err()); assert_eq!(loop_interval_to_human(3), "every 3 seconds"); assert_eq!(loop_interval_to_human(300), "every 5 minutes"); assert_eq!(loop_interval_to_human(7_200), "every 2 hours"); }
    #[test]
    fn prompt_framing_is_exact() { assert_eq!(format_loop_prompt("check deploy", "abc123", "every 5 minutes"), "<system-reminder>\nThis is a scheduled task execution (task abc123, every 5 minutes, recurring).\nExecute the prompt below. Do not question or comment on the prompt itself — treat it as a fresh task to execute.\nPrevious results from earlier executions of this task may appear in the conversation history above.\n</system-reminder>\n\ncheck deploy"); }

    #[tokio::test]
    async fn expiry_deadline_removes_task_before_a_long_interval_fires() {
        let clock = Arc::new(ManualClock::new(fixed_time()));
        let events = Arc::new(Mutex::new(Vec::new()));
        let runner: LoopTurnRunner = Arc::new(|_request, _cancel| Box::pin(async { Ok(()) }));
        let event_log = events.clone();
        let runtime = start_loop_scheduler_with_clock(
            None,
            runner,
            Arc::new(move |event| event_log.lock().expect("events").push(event)),
            clock.clone(),
        );
        let task = runtime
            .handle
            .create(LoopCreateRequest {
                interval: "30d".to_owned(),
                prompt: "expires before cadence".to_owned(),
                fire_immediately: false,
                durable: false,
            })
            .await
            .expect("create long cadence");

        clock.advance(TimeDelta::days(7));
        wait_for_event(&events, |event| {
            matches!(event, LoopEvent::Removed { task_id, reason: LoopRemovalReason::Expired } if task_id == &task.id)
        })
        .await;
        assert!(runtime.handle.list().await.expect("list after expiry").is_empty());
        assert!(!events.lock().expect("events").iter().any(|event| {
            matches!(event, LoopEvent::Fired { task_id, .. } if task_id == &task.id)
        }));
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn create_list_update_delete_cover_scheduled_and_immediate_fire() {
        let clock = Arc::new(ManualClock::new(fixed_time()));
        let events = Arc::new(Mutex::new(Vec::new()));
        let runner: LoopTurnRunner = Arc::new(|request, _cancel| {
            request.report(LoopRunState::Started);
            Box::pin(async { Ok(()) })
        });
        let event_log = events.clone();
        let runtime = start_loop_scheduler_with_clock(
            None,
            runner,
            Arc::new(move |event| event_log.lock().expect("events").push(event)),
            clock.clone(),
        );

        let scheduled = runtime
            .handle
            .create(LoopCreateRequest {
                interval: "2m".to_owned(),
                prompt: "scheduled".to_owned(),
                fire_immediately: false,
                durable: false,
            })
            .await
            .expect("scheduled create");
        assert_eq!(runtime.handle.list().await.expect("list"), vec![scheduled.clone()]);
        assert!(!events.lock().expect("events").iter().any(|event| {
            matches!(event, LoopEvent::Fired { task_id, .. } if task_id == &scheduled.id)
        }));

        let updated = runtime
            .handle
            .update(LoopUpdateRequest {
                task_id: scheduled.id.clone(),
                interval: Some("1m".to_owned()),
                prompt: Some("updated prompt".to_owned()),
            })
            .await
            .expect("update");
        assert_eq!(updated.interval_secs, 60);
        assert_eq!(updated.prompt, "updated prompt");
        assert!(events.lock().expect("events").iter().any(|event| {
            matches!(event, LoopEvent::Updated { task } if task.id == scheduled.id)
        }));

        clock.advance(TimeDelta::minutes(1));
        wait_for_event(&events, |event| {
            matches!(event, LoopEvent::Fired { task_id, .. } if task_id == &scheduled.id)
        })
        .await;
        wait_for_event(&events, |event| {
            matches!(event, LoopEvent::Finished { task_id, .. } if task_id == &scheduled.id)
        })
        .await;
        assert!(runtime.handle.delete(&scheduled.id).await.expect("delete"));
        assert!(runtime.handle.list().await.expect("empty after delete").is_empty());

        let immediate = runtime
            .handle
            .create(LoopCreateRequest::immediate("1m", "immediate"))
            .await
            .expect("immediate create");
        wait_for_event(&events, |event| {
            matches!(event, LoopEvent::Fired { task_id, .. } if task_id == &immediate.id)
        })
        .await;
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn suspend_cancels_active_and_queue_then_restores_new_session_state() {
        let directory = tempfile::tempdir().expect("tempdir");
        let first_session = directory.path().join("first.jsonl");
        let second_session = directory.path().join("second.jsonl");
        tokio::fs::write(&first_session, b"").await.expect("first session");
        tokio::fs::write(&second_session, b"").await.expect("second session");
        let clock = Arc::new(ManualClock::new(fixed_time()));
        let events = Arc::new(Mutex::new(Vec::new()));
        let runner: LoopTurnRunner = Arc::new(|request, cancel| {
            request.report(LoopRunState::Started);
            Box::pin(async move {
                cancel.cancelled().await;
                Err("cancelled".to_owned())
            })
        });
        let event_log = events.clone();
        let runtime = start_loop_scheduler_with_clock(
            Some(loop_state_path(&first_session)),
            runner,
            Arc::new(move |event| event_log.lock().expect("events").push(event)),
            clock,
        );
        let active = runtime
            .handle
            .create(LoopCreateRequest::immediate("1m", "active"))
            .await
            .expect("active create");
        wait_for_event(&events, |event| {
            matches!(event, LoopEvent::Fired { task_id, .. } if task_id == &active.id)
        })
        .await;
        let queued = runtime
            .handle
            .create(LoopCreateRequest::immediate("1m", "queued"))
            .await
            .expect("queued create");
        wait_for_event(&events, |event| {
            matches!(event, LoopEvent::Queued { task_id, .. } if task_id == &queued.id)
        })
        .await;

        runtime
            .handle
            .suspend(LoopRemovalReason::SessionChanged)
            .await
            .expect("suspend");
        assert!(matches!(runtime.handle.list().await, Err(LoopSchedulerError::Inactive)));
        assert!(events.lock().expect("events").iter().any(|event| {
            matches!(event, LoopEvent::Removed { task_id, reason: LoopRemovalReason::SessionChanged } if task_id == &active.id)
        }));
        assert!(events.lock().expect("events").iter().any(|event| {
            matches!(event, LoopEvent::Removed { task_id, reason: LoopRemovalReason::SessionChanged } if task_id == &queued.id)
        }));

        runtime
            .handle
            .activate(Some(second_session))
            .await
            .expect("activate second session");
        assert!(runtime.handle.list().await.expect("second session list").is_empty());
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn fake_clock_fires_queues_skips_and_continues_after_failure() {
        let clock = Arc::new(ManualClock::new(fixed_time())); let events = Arc::new(Mutex::new(Vec::new())); let gates = Arc::new(Mutex::new(VecDeque::<oneshot::Receiver<Result<(), String>>>::new())); let (first_tx, first_rx) = oneshot::channel(); let (second_tx, second_rx) = oneshot::channel(); gates.lock().expect("gates").extend([first_rx, second_rx]); let runner_gates = gates.clone(); let runner: LoopTurnRunner = Arc::new(move |request, cancel| { request.report(LoopRunState::Started); let receiver = runner_gates.lock().expect("gates").pop_front().expect("run gate"); Box::pin(async move { tokio::select! { _ = cancel.cancelled() => Err("cancelled".to_owned()), result = receiver => result.expect("gate sender") } }) }); let event_log = events.clone(); let runtime = start_loop_scheduler_with_clock(None, runner, Arc::new(move |event| event_log.lock().expect("events").push(event)), clock.clone());
        let first = runtime.handle.create(LoopCreateRequest::immediate("1m", "first")).await.expect("create first"); wait_for_event(&events, |event| matches!(event, LoopEvent::Fired { task_id, .. } if task_id == &first.id)).await; let second = runtime.handle.create(LoopCreateRequest::immediate("1m", "second")).await.expect("create second"); wait_for_event(&events, |event| matches!(event, LoopEvent::Queued { task_id, .. } if task_id == &second.id)).await; clock.advance(TimeDelta::minutes(1)); wait_for_event(&events, |event| matches!(event, LoopEvent::Skipped { task_id, reason: LoopSkipReason::AlreadyRunning, .. } if task_id == &first.id)).await; wait_for_event(&events, |event| matches!(event, LoopEvent::Skipped { task_id, reason: LoopSkipReason::AlreadyQueued, .. } if task_id == &second.id)).await; first_tx.send(Err("provider unavailable".to_owned())).expect("finish first"); wait_for_event(&events, |event| matches!(event, LoopEvent::Failed { task_id, message, .. } if task_id == &first.id && message == "provider unavailable")).await; wait_for_event(&events, |event| matches!(event, LoopEvent::Fired { task_id, .. } if task_id == &second.id)).await; second_tx.send(Ok(())).expect("finish second"); wait_for_event(&events, |event| matches!(event, LoopEvent::Finished { task_id, .. } if task_id == &second.id)).await; runtime.shutdown().await;
    }

    #[tokio::test]
    async fn cancel_limit_expiry_and_shutdown_are_truthful() {
        let clock = Arc::new(ManualClock::new(fixed_time()));
        let events = Arc::new(Mutex::new(Vec::new()));
        let runner: LoopTurnRunner = Arc::new(|request, cancel| {
            request.report(LoopRunState::Started);
            Box::pin(async move {
                cancel.cancelled().await;
                Err("cancelled".to_owned())
            })
        });
        let event_log = events.clone();
        let runtime = start_loop_scheduler_with_clock(
            None,
            runner,
            Arc::new(move |event| event_log.lock().expect("events").push(event)),
            clock.clone(),
        );
        let active = runtime
            .handle
            .create(LoopCreateRequest::immediate("1m", "active"))
            .await
            .expect("create active");
        wait_for_event(&events, |event| {
            matches!(event, LoopEvent::Fired { task_id, .. } if task_id == &active.id)
        })
        .await;
        assert!(runtime.handle.cancel(&active.id).await.expect("cancel"));
        assert!(!runtime.handle.delete("missing").await.expect("missing"));
        for index in 0..MAX_LOOP_TASKS {
            runtime
                .handle
                .create(LoopCreateRequest {
                    interval: "1d".to_owned(),
                    prompt: format!("task {index}"),
                    fire_immediately: false,
                    durable: false,
                })
                .await
                .expect("within task limit");
        }
        assert!(matches!(
            runtime
                .handle
                .create(LoopCreateRequest::immediate("1h", "overflow"))
                .await,
            Err(LoopSchedulerError::TaskLimitReached(MAX_LOOP_TASKS))
        ));
        clock.advance(TimeDelta::days(8));
        wait_for_event(&events, |event| {
            matches!(event, LoopEvent::Removed { reason: LoopRemovalReason::Expired, .. })
        })
        .await;
        let shutdown_task = runtime
            .handle
            .create(LoopCreateRequest {
                interval: "1d".to_owned(),
                prompt: "remain until shutdown".to_owned(),
                fire_immediately: false,
                durable: true,
            })
            .await
            .expect("create shutdown task");
        runtime.shutdown().await;
        assert!(events.lock().expect("events").iter().any(|event| matches!(
            event,
            LoopEvent::Removed { task_id, reason: LoopRemovalReason::Shutdown }
                if task_id == &shutdown_task.id
        )));
    }
    /// Contract: `cancel_active_iteration` cancels the in-flight iteration
    /// without removing the task or emitting a `Failed` event — the loop
    /// remains scheduled. A real provider/runtime failure (runner returns `Err`
    /// with the token uncancelled) still surfaces as `LoopEvent::Failed`.
    ///
    /// Plausible bug: Esc during a loop turn reports `Loop <id> failed: Request
    /// was aborted` because the abort only cancelled the session, not the loop
    /// iteration token, so the scheduler could not distinguish user abort from
    /// a real failure.
    #[tokio::test]
    async fn cancel_active_iteration_settles_silently_but_real_failure_reports() {
        let clock = Arc::new(ManualClock::new(fixed_time()));
        let events: Arc<Mutex<Vec<LoopEvent>>> = Arc::new(Mutex::new(Vec::new()));
        // The runner blocks until the iteration token is cancelled, mirroring an
        // in-flight turn that observes the user abort.
        let runner: LoopTurnRunner = Arc::new(|request, cancel| {
            request.report(LoopRunState::Started);
            Box::pin(async move {
                cancel.cancelled().await;
                Err("loop cancelled".to_owned())
            })
        });
        let event_log = events.clone();
        let runtime = start_loop_scheduler_with_clock(
            None,
            runner,
            Arc::new(move |event| event_log.lock().expect("events").push(event)),
            clock.clone(),
        );
        let active = runtime
            .handle
            .create(LoopCreateRequest::immediate("1m", "active"))
            .await
            .expect("create active");
        wait_for_event(&events, |event| {
            matches!(event, LoopEvent::Fired { task_id, .. } if task_id == &active.id)
        })
        .await;

        // Esc during the loop-owned turn cancels only the active iteration.
        assert!(runtime
            .handle
            .cancel_active_iteration()
            .await
            .expect("cancel active"));
        // No active run remains to cancel.
        assert!(!runtime
            .handle
            .cancel_active_iteration()
            .await
            .expect("no active run"));

        // The cancelled iteration emits no terminal event (Finished/Failed),
        // and the task remains scheduled for its next cadence fire.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let logged = events.lock().expect("events").clone();
        assert!(
            !logged.iter().any(|event| matches!(
                event,
                LoopEvent::Failed { task_id, .. } if task_id == &active.id
            )),
            "user abort must not emit LoopEvent::Failed: {logged:?}"
        );
        assert!(
            !logged.iter().any(|event| matches!(
                event,
                LoopEvent::Finished { task_id, .. } if task_id == &active.id
            )),
            "cancelled iteration must not emit LoopEvent::Finished: {logged:?}"
        );
        // The task remains scheduled (only the active iteration was cancelled);
        // it has been mutated by the fire (last_fired_at/run_count) so compare
        // by id rather than exact equality with the pre-fire snapshot.
        let remaining = runtime.handle.list().await.expect("task remains");
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, active.id);
        assert_eq!(remaining[0].prompt, active.prompt);

        // A real provider/runtime failure (runner returns Err with the token
        // uncancelled) must still surface as LoopEvent::Failed.
        let failure_events: Arc<Mutex<Vec<LoopEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let failure_log = failure_events.clone();
        let failure_runtime = start_loop_scheduler_with_clock(
            None,
            Arc::new(|request, _cancel| {
                request.report(LoopRunState::Started);
                Box::pin(async { Err("provider unavailable".to_owned()) })
            }),
            Arc::new(move |event| failure_log.lock().expect("events").push(event)),
            clock.clone(),
        );
        let failing = failure_runtime
            .handle
            .create(LoopCreateRequest::immediate("1m", "failing"))
            .await
            .expect("create failing");
        wait_for_event(&failure_events, |event| {
            matches!(event, LoopEvent::Failed { task_id, message, .. } if task_id == &failing.id && message == "provider unavailable")
        })
        .await;

        runtime.shutdown().await;
        failure_runtime.shutdown().await;
    }

    #[tokio::test]
    async fn durable_tasks_persist_resume_and_ephemeral_tasks_do_not() {
        let directory = tempfile::tempdir().expect("tempdir"); let session = directory.path().join("session.jsonl"); tokio::fs::write(&session, b"").await.expect("session file"); let clock = Arc::new(ManualClock::new(fixed_time())); let runner: LoopTurnRunner = Arc::new(|_request, _cancel| Box::pin(async { Ok(()) })); let runtime = start_loop_scheduler_with_clock(Some(loop_state_path(&session)), runner.clone(), Arc::new(|_| {}), clock.clone()); let durable = runtime.handle.create(LoopCreateRequest { interval: "1h".to_owned(), prompt: "durable".to_owned(), fire_immediately: false, durable: true }).await.expect("durable"); runtime.handle.create(LoopCreateRequest { interval: "1h".to_owned(), prompt: "ephemeral".to_owned(), fire_immediately: false, durable: false }).await.expect("ephemeral"); runtime.shutdown().await; let restored_events = Arc::new(Mutex::new(Vec::new())); let restored_log = restored_events.clone(); let restored = start_loop_scheduler_with_clock(Some(loop_state_path(&session)), runner, Arc::new(move |event| restored_log.lock().expect("events").push(event)), clock); wait_for_event(&restored_events, |event| matches!(event, LoopEvent::Created { task, restored: true } if task.id == durable.id)).await; let tasks = restored.handle.list().await.expect("list restored"); assert_eq!(tasks.len(), 1); assert_eq!(tasks[0].prompt, "durable"); restored.shutdown().await;
    }

    #[tokio::test]
    async fn prepare_rejects_malformed_target_sidecar_without_actor_mutation() {
        let directory = tempfile::tempdir().expect("tempdir");
        let source = directory.path().join("source.jsonl");
        let target = directory.path().join("target.jsonl");
        tokio::fs::write(&source, b"").await.expect("source session");
        tokio::fs::write(&target, b"").await.expect("target session");
        tokio::fs::write(loop_state_path(&target), b"{not-json")
            .await
            .expect("malformed sidecar");

        let events = Arc::new(Mutex::new(Vec::new()));
        let event_log = events.clone();
        let runtime = start_loop_scheduler_with_clock(
            Some(loop_state_path(&source)),
            Arc::new(|_request, _cancel| Box::pin(async { Ok(()) })),
            Arc::new(move |event| event_log.lock().expect("events").push(event)),
            Arc::new(ManualClock::new(fixed_time())),
        );
        let source_task = runtime
            .handle
            .create(LoopCreateRequest {
                interval: "1h".to_owned(),
                prompt: "keep me".to_owned(),
                fire_immediately: false,
                durable: true,
            })
            .await
            .expect("source task");

        let prepare_error = prepare_loop_activation(Some(&target))
            .await
            .expect_err("malformed target must fail prepare");
        assert!(
            matches!(prepare_error, LoopSchedulerError::Decode(_)),
            "expected decode error, got {prepare_error:?}"
        );

        let listed = runtime.handle.list().await.expect("source still active");
        assert_eq!(listed, vec![source_task.clone()]);
        assert!(
            !events.lock().expect("events").iter().any(|event| {
                matches!(
                    event,
                    LoopEvent::Removed {
                        reason: LoopRemovalReason::SessionChanged,
                        ..
                    }
                )
            }),
            "prepare must not emit session-change removals: {:?}",
            events.lock().expect("events")
        );
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn commit_session_switch_preserves_old_state_when_old_persist_fails() {
        let directory = tempfile::tempdir().expect("tempdir");
        let source = directory.path().join("source.jsonl");
        let target = directory.path().join("target.jsonl");
        tokio::fs::write(&source, b"").await.expect("source session");
        tokio::fs::write(&target, b"").await.expect("target session");

        let target_task = LoopTask {
            id: "targettask01".to_owned(),
            interval_secs: 3_600,
            prompt: "from target".to_owned(),
            durable: true,
            created_at: fixed_time(),
            last_fired_at: None,
            expires_at: fixed_time() + TimeDelta::days(LOOP_EXPIRY_DAYS),
            run_count: 0,
        };
        save_loop_state(
            &loop_state_path(&target),
            &PersistedLoopState {
                version: LOOP_STATE_VERSION,
                tasks: vec![target_task.clone()],
            },
        )
        .await
        .expect("seed target sidecar");

        let events = Arc::new(Mutex::new(Vec::new()));
        let event_log = events.clone();
        let persist_fail = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let runtime = start_loop_scheduler_with_clock_and_persist_hook(
            Some(loop_state_path(&source)),
            Arc::new(|_request, _cancel| Box::pin(async { Ok(()) })),
            Arc::new(move |event| event_log.lock().expect("events").push(event)),
            Arc::new(ManualClock::new(fixed_time())),
            Some(persist_fail.clone()),
        );
        let source_task = runtime
            .handle
            .create(LoopCreateRequest {
                interval: "1h".to_owned(),
                prompt: "source durable".to_owned(),
                fire_immediately: false,
                durable: true,
            })
            .await
            .expect("source durable");

        let prepared = prepare_loop_activation(Some(&target))
            .await
            .expect("target prepare succeeds");
        let before_commit = events.lock().expect("events").len();

        // Trip the commit-gate persist after prepare has already validated the target.
        persist_fail.store(true, std::sync::atomic::Ordering::SeqCst);
        let persist_error = runtime
            .handle
            .commit_session_switch(prepared, LoopRemovalReason::SessionChanged)
            .await
            .expect_err("commit must fail when old persist fails");
        assert!(
            matches!(persist_error, LoopSchedulerError::Persistence(_)),
            "expected persistence error, got {persist_error:?}"
        );

        let listed = runtime.handle.list().await.expect("old session intact");
        assert_eq!(listed, vec![source_task.clone()]);
        {
            let after = events.lock().expect("events");
            assert_eq!(
                after.len(),
                before_commit,
                "failed commit must emit no transition events: {after:?}"
            );
            assert!(
                !after.iter().any(|event| {
                    matches!(
                        event,
                        LoopEvent::Created {
                            task,
                            restored: true
                        } if task.id == target_task.id
                    )
                }),
                "no target restore events before successful commit: {after:?}"
            );
            assert!(
                !after.iter().any(|event| {
                    matches!(
                        event,
                        LoopEvent::Removed {
                            reason: LoopRemovalReason::SessionChanged,
                            ..
                        }
                    )
                }),
                "failed commit must not remove old tasks: {after:?}"
            );
        }
        persist_fail.store(false, std::sync::atomic::Ordering::SeqCst);
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn commit_session_switch_cancels_work_restores_target_and_orders_events() {
        let directory = tempfile::tempdir().expect("tempdir");
        let source = directory.path().join("source.jsonl");
        let target = directory.path().join("target.jsonl");
        tokio::fs::write(&source, b"").await.expect("source session");
        tokio::fs::write(&target, b"").await.expect("target session");

        let target_task = LoopTask {
            id: "targetrest01".to_owned(),
            interval_secs: 3_600,
            prompt: "restored target".to_owned(),
            durable: true,
            created_at: fixed_time(),
            last_fired_at: None,
            expires_at: fixed_time() + TimeDelta::days(LOOP_EXPIRY_DAYS),
            run_count: 2,
        };
        save_loop_state(
            &loop_state_path(&target),
            &PersistedLoopState {
                version: LOOP_STATE_VERSION,
                tasks: vec![target_task.clone()],
            },
        )
        .await
        .expect("seed target sidecar");

        let events = Arc::new(Mutex::new(Vec::new()));
        let event_log = events.clone();
        let runtime = start_loop_scheduler_with_clock(
            Some(loop_state_path(&source)),
            Arc::new(|request, cancel| {
                request.report(LoopRunState::Started);
                Box::pin(async move {
                    cancel.cancelled().await;
                    Err("cancelled".to_owned())
                })
            }),
            Arc::new(move |event| event_log.lock().expect("events").push(event)),
            Arc::new(ManualClock::new(fixed_time())),
        );

        let active = runtime
            .handle
            .create(LoopCreateRequest::immediate("1m", "active source"))
            .await
            .expect("active create");
        wait_for_event(&events, |event| {
            matches!(event, LoopEvent::Fired { task_id, .. } if task_id == &active.id)
        })
        .await;
        let queued = runtime
            .handle
            .create(LoopCreateRequest::immediate("1m", "queued source"))
            .await
            .expect("queued create");
        wait_for_event(&events, |event| {
            matches!(event, LoopEvent::Queued { task_id, .. } if task_id == &queued.id)
        })
        .await;
        let durable_source = runtime
            .handle
            .create(LoopCreateRequest {
                interval: "1h".to_owned(),
                prompt: "durable source".to_owned(),
                fire_immediately: false,
                durable: true,
            })
            .await
            .expect("durable source");

        let prepared = prepare_loop_activation_at(Some(&target), fixed_time())
            .await
            .expect("prepare target");
        let marker = events.lock().expect("events").len();
        // Prepare alone must not emit target events.
        assert!(
            !events.lock().expect("events").iter().any(|event| {
                matches!(
                    event,
                    LoopEvent::Created {
                        task,
                        restored: true
                    } if task.id == target_task.id
                )
            }),
            "no target events before commit"
        );

        runtime
            .handle
            .commit_session_switch(prepared, LoopRemovalReason::SessionChanged)
            .await
            .expect("atomic switch");

        let listed = runtime.handle.list().await.expect("target list");
        assert_eq!(listed, vec![target_task.clone()]);

        // Source durable must have been flushed before cutover.
        let source_state = load_loop_state(&loop_state_path(&source))
            .await
            .expect("reload source sidecar");
        assert!(
            source_state
                .tasks
                .iter()
                .any(|task| task.id == durable_source.id && task.prompt == "durable source"),
            "old durable state persisted before commit: {source_state:?}"
        );

        let transition: Vec<LoopEvent> = events
            .lock()
            .expect("events")
            .iter()
            .skip(marker)
            .cloned()
            .collect();
        let removed_ids: Vec<&str> = transition
            .iter()
            .filter_map(|event| match event {
                LoopEvent::Removed {
                    task_id,
                    reason: LoopRemovalReason::SessionChanged,
                } => Some(task_id.as_str()),
                _ => None,
            })
            .collect();
        assert!(removed_ids.contains(&active.id.as_str()));
        assert!(removed_ids.contains(&queued.id.as_str()));
        assert!(removed_ids.contains(&durable_source.id.as_str()));

        let first_target = transition.iter().position(|event| {
            matches!(
                event,
                LoopEvent::Created {
                    task,
                    restored: true
                } if task.id == target_task.id
            )
        });
        let last_removal = transition.iter().rposition(|event| {
            matches!(
                event,
                LoopEvent::Removed {
                    reason: LoopRemovalReason::SessionChanged,
                    ..
                }
            )
        });
        let (first_target, last_removal) = (
            first_target.expect("target restored event"),
            last_removal.expect("session removal events"),
        );
        assert!(
            last_removal < first_target,
            "all SessionChanged removals must precede target restore: {transition:?}"
        );
        assert!(
            !transition.iter().any(|event| {
                matches!(event, LoopEvent::Finished { task_id, .. } if task_id == &active.id)
                    || matches!(event, LoopEvent::Failed { task_id, .. } if task_id == &active.id)
            }),
            "cancelled active run must not report completion: {transition:?}"
        );

        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn prepare_none_session_then_commit_clears_to_ephemeral_runtime() {
        let directory = tempfile::tempdir().expect("tempdir");
        let source = directory.path().join("source.jsonl");
        tokio::fs::write(&source, b"").await.expect("source session");
        let events = Arc::new(Mutex::new(Vec::new()));
        let event_log = events.clone();
        let runtime = start_loop_scheduler_with_clock(
            Some(loop_state_path(&source)),
            Arc::new(|_request, _cancel| Box::pin(async { Ok(()) })),
            Arc::new(move |event| event_log.lock().expect("events").push(event)),
            Arc::new(ManualClock::new(fixed_time())),
        );
        let task = runtime
            .handle
            .create(LoopCreateRequest {
                interval: "1h".to_owned(),
                prompt: "ephemeral clear".to_owned(),
                fire_immediately: false,
                durable: false,
            })
            .await
            .expect("create");
        let prepared = prepare_loop_activation(None)
            .await
            .expect("prepare none");
        runtime
            .handle
            .commit_session_switch(prepared, LoopRemovalReason::SessionChanged)
            .await
            .expect("switch to no session file");
        assert!(runtime.handle.list().await.expect("empty after switch").is_empty());
        assert!(events.lock().expect("events").iter().any(|event| {
            matches!(
                event,
                LoopEvent::Removed {
                    task_id,
                    reason: LoopRemovalReason::SessionChanged
                } if task_id == &task.id
            )
        }));
        runtime.shutdown().await;
    }


    fn fixed_time() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-01-02T03:04:05Z")
            .expect("fixed time")
            .with_timezone(&Utc)
    }


    async fn wait_for_event(
        events: &Mutex<Vec<LoopEvent>>,
        predicate: impl Fn(&LoopEvent) -> bool,
    ) {
        for _ in 0..200 {
            if events.lock().expect("events").iter().any(&predicate) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!(
            "expected loop event was not observed: {:?}",
            events.lock().expect("events")
        );
    }
    struct ManualClock { now: Mutex<DateTime<Utc>>, changed: Arc<Notify> }
    impl ManualClock { fn new(now: DateTime<Utc>) -> Self { Self { now: Mutex::new(now), changed: Arc::new(Notify::new()) } } fn advance(&self, delta: TimeDelta) { *self.now.lock().expect("clock") += delta; self.changed.notify_waiters(); } }
    impl LoopClock for ManualClock { fn now(&self) -> DateTime<Utc> { *self.now.lock().expect("clock") } fn sleep_until(&self, deadline: DateTime<Utc>) -> BoxFuture<()> { let now = self.now(); let changed = self.changed.clone(); if now >= deadline { Box::pin(async {}) } else { Box::pin(async move { changed.notified().await; }) } } }
}
