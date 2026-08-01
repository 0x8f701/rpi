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

const MINIMUM_INTERVAL_SECS: u64 = 60;
const MAX_LOOP_TASKS: usize = 50;
const LOOP_EXPIRY_DAYS: i64 = 7;
const LOOP_STATE_VERSION: u32 = 1;
type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParsedLoopArgs<'a> { pub interval: Option<&'a str>, pub prompt: &'a str }

/// Extract an unambiguous leading compact interval. Natural-language schedules
/// remain in the prompt so adapters never invent a default.
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
    if value.len() < 2 { return false; }
    let (digits, suffix) = value.split_at(value.len() - 1);
    matches!(suffix, "s" | "m" | "h" | "d") && digits.chars().all(|c| c.is_ascii_digit()) && digits.parse::<u64>().is_ok_and(|n| n > 0)
}

pub fn parse_loop_interval(value: &str) -> Result<u64, LoopSchedulerError> {
    let value = value.trim();
    if value.is_empty() { return Err(LoopSchedulerError::InvalidInterval("interval cannot be empty".into())); }
    let (digits, suffix) = value.split_at(value.len() - 1);
    let number = digits.parse::<u64>().map_err(|_| LoopSchedulerError::InvalidInterval(format!("invalid interval format: {value:?} (expected e.g. 5m, 2h, 1d)")))?;
    if number == 0 { return Err(LoopSchedulerError::InvalidInterval("interval value must be greater than 0".into())); }
    let unit = match suffix {
        "s" => 1, "m" => 60, "h" => 3_600, "d" => 86_400,
        _ => return Err(LoopSchedulerError::InvalidInterval(format!("invalid interval suffix: {suffix:?} (expected s, m, h, or d)"))),
    };
    number.checked_mul(unit).map(|seconds| seconds.max(MINIMUM_INTERVAL_SECS)).ok_or_else(|| LoopSchedulerError::InvalidInterval(format!("interval too large: {value:?}")))
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
pub const fn loop_usage_message() -> &'static str { "Usage: /loop [interval] <prompt>\nExample: /loop 30m check deploy status\nExample: /loop check deploy status every hour\n\nTell me how often it should run (e.g. 30m, 1 hour, every 2 days)." }

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

#[derive(Clone)]
pub struct LoopSchedulerHandle { commands: mpsc::UnboundedSender<LoopCommand> }
impl LoopSchedulerHandle {
    pub async fn create(&self, request: LoopCreateRequest) -> Result<LoopTask, LoopSchedulerError> { let (reply, response) = oneshot::channel(); self.send(LoopCommand::Create { request, reply })?; receive(response).await }
    pub async fn update(&self, request: LoopUpdateRequest) -> Result<LoopTask, LoopSchedulerError> { let (reply, response) = oneshot::channel(); self.send(LoopCommand::Update { request, reply })?; receive(response).await }
    pub async fn list(&self) -> Result<Vec<LoopTask>, LoopSchedulerError> { let (reply, response) = oneshot::channel(); self.send(LoopCommand::List { reply })?; receive(response).await }
    pub async fn delete(&self, task_id: &str) -> Result<bool, LoopSchedulerError> { self.delete_inner(task_id, false).await }
    pub async fn cancel(&self, task_id: &str) -> Result<bool, LoopSchedulerError> { self.delete_inner(task_id, true).await }
    async fn delete_inner(&self, task_id: &str, cancel_active: bool) -> Result<bool, LoopSchedulerError> { let (reply, response) = oneshot::channel(); self.send(LoopCommand::Delete { task_id: task_id.to_owned(), cancel_active, reply })?; receive(response).await }
    pub(crate) async fn suspend(&self, reason: LoopRemovalReason) -> Result<(), LoopSchedulerError> { let (reply, response) = oneshot::channel(); self.send(LoopCommand::Suspend { reason, reply })?; receive(response).await }
    pub(crate) async fn activate(&self, session_file: Option<PathBuf>) -> Result<(), LoopSchedulerError> { let (reply, response) = oneshot::channel(); self.send(LoopCommand::Activate { storage_path: session_file.as_deref().map(loop_state_path), reply })?; receive(response).await }
    fn send(&self, command: LoopCommand) -> Result<(), LoopSchedulerError> { self.commands.send(command).map_err(|_| LoopSchedulerError::Stopped) }
}
async fn receive<T>(response: oneshot::Receiver<Result<T, LoopSchedulerError>>) -> Result<T, LoopSchedulerError> { response.await.map_err(|_| LoopSchedulerError::Stopped)? }

pub(crate) struct LoopRunRequest {
    pub task_id: String,
    pub prompt: String,
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
fn start_loop_scheduler_with_clock(storage_path: Option<PathBuf>, runner: LoopTurnRunner, events: LoopEventSink, clock: Arc<dyn LoopClock>) -> LoopSchedulerRuntime {
    let (commands, command_rx) = mpsc::unbounded_channel(); let (completions, completion_rx) = mpsc::unbounded_channel(); let cancel = CancellationToken::new();
    let actor = LoopSchedulerActor { tasks: Vec::new(), queue: VecDeque::new(), active: None, command_rx, completions, completion_rx, runner, events, clock, cancel: cancel.clone(), storage_path, active_session: true, session_generation: 0, pending_suspend: None };
    let join = tokio::spawn(actor.run()); LoopSchedulerRuntime { handle: LoopSchedulerHandle { commands }, cancel, join }
}

enum LoopCommand {
    Create { request: LoopCreateRequest, reply: oneshot::Sender<Result<LoopTask, LoopSchedulerError>> }, Update { request: LoopUpdateRequest, reply: oneshot::Sender<Result<LoopTask, LoopSchedulerError>> }, List { reply: oneshot::Sender<Result<Vec<LoopTask>, LoopSchedulerError>> }, Delete { task_id: String, cancel_active: bool, reply: oneshot::Sender<Result<bool, LoopSchedulerError>> }, Suspend { reason: LoopRemovalReason, reply: oneshot::Sender<Result<(), LoopSchedulerError>> }, Activate { storage_path: Option<PathBuf>, reply: oneshot::Sender<Result<(), LoopSchedulerError>> },
}
struct PendingRun { request: LoopRunRequest, next_fire_at: DateTime<Utc> }
struct ActiveRun { task_id: String, cancel: CancellationToken, join: JoinHandle<()>, generation: u64, report_completion: bool }
struct RunCompletion { task_id: String, generation: u64, result: RunResult }
enum RunResult { Finished, Failed(String), Cancelled }
struct PendingSuspend { reply: oneshot::Sender<Result<(), LoopSchedulerError>> }
struct LoopSchedulerActor { tasks: Vec<LoopTask>, queue: VecDeque<PendingRun>, active: Option<ActiveRun>, command_rx: mpsc::UnboundedReceiver<LoopCommand>, completions: mpsc::UnboundedSender<RunCompletion>, completion_rx: mpsc::UnboundedReceiver<RunCompletion>, runner: LoopTurnRunner, events: LoopEventSink, clock: Arc<dyn LoopClock>, cancel: CancellationToken, storage_path: Option<PathBuf>, active_session: bool, session_generation: u64, pending_suspend: Option<PendingSuspend> }

impl LoopSchedulerActor {
    async fn run(mut self) {
        if let Err(error) = self.load_and_announce().await { self.emit_scheduler_error(error); }
        loop { let deadline = self.next_deadline(); tokio::select! { biased; _ = self.cancel.cancelled() => break, completion = self.completion_rx.recv() => if let Some(completion) = completion { self.finish_run(completion); }, command = self.command_rx.recv() => { let Some(command) = command else { break }; self.handle_command(command).await; }, () = sleep_for_deadline(self.clock.clone(), deadline), if self.active_session => self.fire_one_due().await, } }
        self.shutdown().await;
    }
    fn next_deadline(&self) -> Option<DateTime<Utc>> { self.tasks.iter().map(LoopTask::next_fire_at).min() }
    async fn handle_command(&mut self, command: LoopCommand) { match command {
        LoopCommand::Create { request, reply } => { let result = self.create(request).await; let _ = reply.send(result); }, LoopCommand::Update { request, reply } => { let result = self.update(request).await; let _ = reply.send(result); }, LoopCommand::List { reply } => { let result = if self.active_session { Ok(self.tasks.clone()) } else { Err(LoopSchedulerError::Inactive) }; let _ = reply.send(result); }, LoopCommand::Delete { task_id, cancel_active, reply } => { let result = self.delete(&task_id, cancel_active).await; let _ = reply.send(result); }, LoopCommand::Suspend { reason, reply } => self.suspend(reason, reply).await, LoopCommand::Activate { storage_path, reply } => { let result = self.activate(storage_path).await; let _ = reply.send(result); },
    } }
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
    async fn fire_one_due(&mut self) {
        let now = self.clock.now();
        let Some(index) = self.tasks.iter().position(|task| task.next_fire_at() <= now) else {
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

        let request = LoopRunRequest {
            task_id: task.id.clone(),
            prompt: format_loop_prompt(&task.prompt, &task.id, &task.human_schedule()),
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
    }
    async fn suspend(&mut self, reason: LoopRemovalReason, reply: oneshot::Sender<Result<(), LoopSchedulerError>>) {
        if !self.active_session { let _ = reply.send(Ok(())); return; } if let Err(error) = self.persist().await { let _ = reply.send(Err(error)); return; } self.active_session = false; self.session_generation = self.session_generation.wrapping_add(1); self.queue.clear(); if let Some(active) = self.active.as_mut() { active.report_completion = false; active.cancel.cancel(); } let tasks = std::mem::take(&mut self.tasks); for task in tasks { self.emit(LoopEvent::Removed { task_id: task.id, reason }); } if self.active.is_some() { self.pending_suspend = Some(PendingSuspend { reply }); } else { let _ = reply.send(Ok(())); }
    }
    async fn activate(&mut self, storage_path: Option<PathBuf>) -> Result<(), LoopSchedulerError> { if self.active_session || self.active.is_some() || self.pending_suspend.is_some() { return Err(LoopSchedulerError::Inactive); } self.storage_path = storage_path; self.active_session = true; if let Err(error) = self.load_and_announce().await { self.active_session = false; return Err(error); } Ok(()) }
    fn complete_pending_suspend(&mut self) { if self.active.is_none() && let Some(pending) = self.pending_suspend.take() { let _ = pending.reply.send(Ok(())); } }
    async fn load_and_announce(&mut self) -> Result<(), LoopSchedulerError> {
        let Some(path) = &self.storage_path else { return Ok(()); }; let state = load_loop_state(path).await?; let now = self.clock.now(); let mut expired = Vec::new(); self.tasks = state.tasks.into_iter().filter(|task| { if task.is_expired(now) { expired.push(task.id.clone()); false } else { true } }).collect(); if !expired.is_empty() { self.persist().await?; for task_id in expired { self.emit(LoopEvent::Removed { task_id, reason: LoopRemovalReason::Expired }); } } for task in self.tasks.clone() { self.emit(LoopEvent::Created { task, restored: true }); } Ok(())
    }
    async fn persist(&self) -> Result<(), LoopSchedulerError> { let Some(path) = &self.storage_path else { return Ok(()); }; save_loop_state(path, &PersistedLoopState { version: LOOP_STATE_VERSION, tasks: self.tasks.iter().filter(|task| task.durable).cloned().collect() }).await }
    async fn shutdown(&mut self) {
        if let Err(error) = self.persist().await { self.emit_scheduler_error(error); }
        self.queue.clear();
        if let Some(active) = self.active.take() {
            active.cancel.cancel();
            let _ = active.join.await;
        }
        let tasks = std::mem::take(&mut self.tasks);
        for task in tasks {
            self.emit(LoopEvent::Removed { task_id: task.id, reason: LoopRemovalReason::Shutdown });
        }
        if let Some(pending) = self.pending_suspend.take() {
            let _ = pending.reply.send(Ok(()));
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

#[derive(Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PersistedLoopState { version: u32, tasks: Vec<LoopTask> }
async fn load_loop_state(path: &Path) -> Result<PersistedLoopState, LoopSchedulerError> {
    let bytes = match tokio::fs::read(path).await { Ok(bytes) => bytes, Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(PersistedLoopState { version: LOOP_STATE_VERSION, tasks: Vec::new() }), Err(error) => return Err(LoopSchedulerError::Persistence(error)), }; let state = serde_json::from_slice::<PersistedLoopState>(&bytes).map_err(LoopSchedulerError::Decode)?; if state.version != LOOP_STATE_VERSION { return Err(LoopSchedulerError::Decode(serde_json::Error::io(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("unsupported loop state version {}; expected {LOOP_STATE_VERSION}", state.version))))); } Ok(state)
}
async fn save_loop_state(path: &Path, state: &PersistedLoopState) -> Result<(), LoopSchedulerError> {
    let bytes = serde_json::to_vec(state).map_err(LoopSchedulerError::Decode)?; let parent = path.parent().unwrap_or_else(|| Path::new(".")); tokio::fs::create_dir_all(parent).await.map_err(LoopSchedulerError::Persistence)?; let temporary = path.with_extension(format!("tmp-{}", uuid::Uuid::now_v7().simple())); tokio::fs::write(&temporary, bytes).await.map_err(LoopSchedulerError::Persistence)?; if let Err(error) = tokio::fs::rename(&temporary, path).await { let _ = tokio::fs::remove_file(&temporary).await; return Err(LoopSchedulerError::Persistence(error)); } Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use super::*;
    use tokio::sync::Notify;

    #[test]
    fn parser_matches_researched_fixtures() {
        let fixtures = [("5m check deploy status", Some("5m"), "check deploy status"), ("check deploy status", None, "check deploy status"), ("2h run tests", Some("2h"), "run tests"), ("1d daily report", Some("1d"), "daily report"), ("60s ping health", Some("60s"), "ping health"), ("5m", None, "5m"), ("check 5m deploy", None, "check 5m deploy"), ("", None, ""), ("   ", None, ""), ("5x do x", None, "5x do x"), ("5 do x", None, "5 do x"), ("m do x", None, "m do x"), ("55mm do x", None, "55mm do x"), ("0m do x", None, "0m do x"), ("0s do x", None, "0s do x"), ("abc do x", None, "abc do x"), ("99999999999999999999m do x", None, "99999999999999999999m do x"), ("every 30 minutes do x", None, "every 30 minutes do x"), ("30 min check deploy", None, "30 min check deploy"), ("1 hour run report", None, "1 hour run report"), ("run the report every 1h", None, "run the report every 1h")];
        for (input, interval, prompt) in fixtures { assert_eq!(parse_loop_args(input), ParsedLoopArgs { interval, prompt }, "fixture {input:?}"); }
    }
    #[test]
    fn interval_parse_clamps_rejects_and_formats() { assert_eq!(parse_loop_interval("5m").expect("minutes"), 300); assert_eq!(parse_loop_interval("2h").expect("hours"), 7_200); assert_eq!(parse_loop_interval("1d").expect("days"), 86_400); assert_eq!(parse_loop_interval("1s").expect("clamped"), 60); assert!(parse_loop_interval("0m").is_err()); assert!(parse_loop_interval("18446744073709551615d").is_err()); assert_eq!(loop_interval_to_human(60), "every 1 minute"); assert_eq!(loop_interval_to_human(7_200), "every 2 hours"); }
    #[test]
    fn prompt_framing_is_exact() { assert_eq!(format_loop_prompt("check deploy", "abc123", "every 5 minutes"), "<system-reminder>\nThis is a scheduled task execution (task abc123, every 5 minutes, recurring).\nExecute the prompt below. Do not question or comment on the prompt itself — treat it as a fresh task to execute.\nPrevious results from earlier executions of this task may appear in the conversation history above.\n</system-reminder>\n\ncheck deploy"); }

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

    #[tokio::test]
    async fn durable_tasks_persist_resume_and_ephemeral_tasks_do_not() {
        let directory = tempfile::tempdir().expect("tempdir"); let session = directory.path().join("session.jsonl"); tokio::fs::write(&session, b"").await.expect("session file"); let clock = Arc::new(ManualClock::new(fixed_time())); let runner: LoopTurnRunner = Arc::new(|_request, _cancel| Box::pin(async { Ok(()) })); let runtime = start_loop_scheduler_with_clock(Some(loop_state_path(&session)), runner.clone(), Arc::new(|_| {}), clock.clone()); let durable = runtime.handle.create(LoopCreateRequest { interval: "1h".to_owned(), prompt: "durable".to_owned(), fire_immediately: false, durable: true }).await.expect("durable"); runtime.handle.create(LoopCreateRequest { interval: "1h".to_owned(), prompt: "ephemeral".to_owned(), fire_immediately: false, durable: false }).await.expect("ephemeral"); runtime.shutdown().await; let restored_events = Arc::new(Mutex::new(Vec::new())); let restored_log = restored_events.clone(); let restored = start_loop_scheduler_with_clock(Some(loop_state_path(&session)), runner, Arc::new(move |event| restored_log.lock().expect("events").push(event)), clock); wait_for_event(&restored_events, |event| matches!(event, LoopEvent::Created { task, restored: true } if task.id == durable.id)).await; let tasks = restored.handle.list().await.expect("list restored"); assert_eq!(tasks.len(), 1); assert_eq!(tasks[0].prompt, "durable"); restored.shutdown().await;
    }

    fn fixed_time() -> DateTime<Utc> { DateTime::parse_from_rfc3339("2026-01-02T03:04:05Z").expect("fixed time").with_timezone(&Utc) }
    async fn wait_for_event(events: &Mutex<Vec<LoopEvent>>, predicate: impl Fn(&LoopEvent) -> bool) { for _ in 0..1_000 { if events.lock().expect("events").iter().any(&predicate) { return; } tokio::task::yield_now().await; } panic!("expected loop event was not observed: {:?}", events.lock().expect("events")); }
    struct ManualClock { now: Mutex<DateTime<Utc>>, changed: Arc<Notify> }
    impl ManualClock { fn new(now: DateTime<Utc>) -> Self { Self { now: Mutex::new(now), changed: Arc::new(Notify::new()) } } fn advance(&self, delta: TimeDelta) { *self.now.lock().expect("clock") += delta; self.changed.notify_waiters(); } }
    impl LoopClock for ManualClock { fn now(&self) -> DateTime<Utc> { *self.now.lock().expect("clock") } fn sleep_until(&self, deadline: DateTime<Utc>) -> BoxFuture<()> { let now = self.now(); let changed = self.changed.clone(); if now >= deadline { Box::pin(async {}) } else { Box::pin(async move { changed.notified().await; }) } } }
}
