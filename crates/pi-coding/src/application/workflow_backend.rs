use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;

use super::Application;
use crate::{
    JobStatus, MailboxMessage, OrchestrationConcurrencyGate, OrchestrationRuntime,
    PlanningTurnOutcome, Session, TodoApplyResult, TodoDagExecutionOutcome, TodoDagExecutionStatus,
    TodoOp, TodoState, WorkflowJobSnapshot, WorkflowRuntimeScope, WorkflowSupervisorBackend,
};

const WORKFLOW_JOB_DRAIN_TIMEOUT: Duration = Duration::from_secs(10);
const WORKFLOW_JOB_DRAIN_POLL: Duration = Duration::from_millis(10);
/// Maximum assistant turns in one bounded planning prompt (P0-1): the inner
/// agent run is unbounded at the session layer, so the workflow imposes its
/// own budget through the per-turn stop hook.
const PLANNING_MAX_TURNS: usize = 8;
/// Maximum Todo/tool calls in one bounded planning prompt (P0-1).
const PLANNING_MAX_TOOL_CALLS: usize = 16;

/// RAII cleanup for the per-turn planning stop hook: clears the session's
/// hook when dropped, including when the enclosing future is cancelled or
/// dropped mid-await. [`WorkflowSupervisor`]'s `await_planning_turn` drops the
/// pinned planning future on the wall-clock deadline and on semantic
/// non-progress detection (right after `pause`), so hook cleanup must never
/// depend on the async block being polled to completion — a stale hook would
/// silently cap later steer turns at the planning budget.
///
/// [`WorkflowSupervisor`]: crate::workflow::supervisor::WorkflowSupervisor
struct PlanningStopHookGuard {
    session: Session,
    armed: bool,
}

impl Drop for PlanningStopHookGuard {
    fn drop(&mut self) {
        if self.armed {
            self.session.set_should_stop_after_turn(None);
        }
    }
}

/// Supervisor backend pinned to exactly one child Application runtime incarnation.
///
/// Keeping the orchestration runtime and generation beside the Application avoids
/// resolving either from mutable parent state while a lifecycle operation is in flight.
pub(super) struct WorkflowApplicationBackend {
    application: Application,
    orchestration: OrchestrationRuntime,
    workflow_id: String,
    generation: u64,
    /// Wall-clock budget for one planning prompt (0 = default).
    planning_deadline_ms: u64,
}

impl WorkflowApplicationBackend {
    pub(super) fn new(
        application: Application,
        orchestration: OrchestrationRuntime,
        workflow_id: String,
        generation: u64,
        planning_deadline_ms: u64,
    ) -> Result<Self> {
        if workflow_id.trim().is_empty() {
            bail!("workflow id must not be empty");
        }
        Ok(Self {
            application,
            orchestration,
            workflow_id,
            generation,
            planning_deadline_ms,
        })
    }

    pub(super) fn application(&self) -> &Application {
        &self.application
    }

    pub(super) fn orchestration(&self) -> &OrchestrationRuntime {
        &self.orchestration
    }

    fn scoped_jobs(&self) -> Vec<WorkflowJobSnapshot> {
        self.orchestration
            .workflow_jobs(&self.workflow_id, self.generation)
    }

    fn active_scoped_job_ids(&self) -> Vec<String> {
        self.scoped_jobs()
            .into_iter()
            .filter(|job| matches!(job.job.status, JobStatus::Queued | JobStatus::Running))
            .map(|job| job.job.id)
            .collect()
    }

    async fn drain_jobs(&self, ids: &[String]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let deadline = Instant::now() + WORKFLOW_JOB_DRAIN_TIMEOUT;
        loop {
            let active = self
                .orchestration
                .workflow_jobs(&self.workflow_id, self.generation)
                .into_iter()
                .filter(|job| ids.contains(&job.job.id) && !job.job.status.is_settled())
                .map(|job| job.job.id)
                .collect::<Vec<_>>();
            if active.is_empty() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(anyhow!("workflow job drain timed out"));
            }
            tokio::time::sleep(WORKFLOW_JOB_DRAIN_POLL).await;
        }
    }

    pub(super) async fn abort_application(&self) {
        self.application.abort().await;
        self.application.wait_for_idle().await;
    }

    pub(super) async fn cleanup(&self) -> Result<()> {
        let ids = self.active_scoped_job_ids();
        self.orchestration.cancel_jobs(&ids);
        self.abort_application().await;
        let drain_result = self.drain_jobs(&ids).await;
        self.application.cleanup().await;
        drain_result
    }
}

#[async_trait]
impl WorkflowSupervisorBackend for WorkflowApplicationBackend {
    fn todo_state(&self) -> TodoState {
        self.application.todo_state()
    }

    fn todo_dag_status(&self) -> TodoDagExecutionStatus {
        self.application.todo_dag_status()
    }

    fn workflow_jobs(&self, workflow_id: &str, generation: u64) -> Vec<WorkflowJobSnapshot> {
        self.orchestration.workflow_jobs(workflow_id, generation)
    }

    fn inbox(&self, agent_id: &str, peek: bool) -> Vec<MailboxMessage> {
        self.orchestration.inbox(agent_id, peek)
    }

    fn active_workflow_job_ids(&self, workflow_id: &str, generation: u64) -> Vec<String> {
        self.orchestration
            .workflow_jobs(workflow_id, generation)
            .into_iter()
            .filter(|job| matches!(job.job.status, JobStatus::Queued | JobStatus::Running))
            .map(|job| job.job.id)
            .collect()
    }

    fn configure_workflow_runtime(
        &self,
        scope: WorkflowRuntimeScope,
        max_concurrency: usize,
        global_concurrency: OrchestrationConcurrencyGate,
    ) -> Result<()> {
        if scope.workflow_id != self.workflow_id || scope.generation != self.generation {
            bail!("workflow runtime scope mismatch");
        }
        if self.orchestration.max_concurrency() != max_concurrency {
            bail!("workflow concurrency mismatch");
        }
        self.orchestration.set_workflow_scope(scope)?;
        self.orchestration
            .set_global_concurrency_gate(global_concurrency)
    }

    async fn prompt_supervisor(&self, prompt: String) -> Result<PlanningTurnOutcome> {
        // P0-1/P0-2: bound the INNER agent run, not just the outer prompt
        // count. A per-turn stop hook caps assistant turns and tool calls and
        // terminates the run as soon as a valid plan is committed (the first
        // successful `todo init`) once the model stops touching the Todo
        // DAG, so a correcting model can never keep the workflow in Planning
        // by looping forever.
        let turns = Arc::new(AtomicUsize::new(0));
        let tool_calls = Arc::new(AtomicUsize::new(0));
        let plan_committed = Arc::new(AtomicBool::new(false));
        let budget_reached = Arc::new(AtomicBool::new(false));
        let session = self.application.session();
        // The guard (not a trailing `set_should_stop_after_turn(None)`) owns
        // hook cleanup: `await_planning_turn` drops this future mid-await on
        // the deadline/non-progress abort, and the cleanup must survive that
        // drop so a stale hook never caps later steer turns.
        let _hook_guard = PlanningStopHookGuard {
            session: session.clone(),
            armed: true,
        };
        let hook_turns = turns.clone();
        let hook_tool_calls = tool_calls.clone();
        let hook_plan_committed = plan_committed.clone();
        let hook_budget_reached = budget_reached.clone();
        session.set_should_stop_after_turn(Some(Arc::new(move |context| {
            let turn_tool_calls = context
                .message
                .content
                .iter()
                .filter(|block| matches!(block, pi_ai::ContentBlock::ToolCall(_)))
                .count();
            let turn_count = hook_turns.fetch_add(1, Ordering::Relaxed) + 1;
            let call_count = hook_tool_calls.fetch_add(turn_tool_calls, Ordering::Relaxed)
                + turn_tool_calls;
            if !hook_plan_committed.load(Ordering::Acquire)
                && context
                    .tool_results
                    .iter()
                    .any(crate::workflow_supervisor_todo_init_succeeded)
            {
                hook_plan_committed.store(true, Ordering::Release);
            }
            // The bound trips only on a turn that would otherwise continue
            // (one that made tool calls); a natural final text turn is never
            // mislabeled as a budget stop.
            let reached = (turn_tool_calls > 0
                && (turn_count >= PLANNING_MAX_TURNS || call_count >= PLANNING_MAX_TOOL_CALLS));
            if reached {
                hook_budget_reached.store(true, Ordering::Release);
            }
            // Plan-commit stop carries a short grace: the run continues while
            // the model still mutates the Todo DAG (dependency calls after
            // init) and stops at the first turn that leaves Todo-land.
            (hook_plan_committed.load(Ordering::Acquire) && turn_tool_calls == 0) || reached
        })));

        let mut events = self.application.subscribe();
        let outcome = async {
            self.application
                .prompt_without_natural_language_spawn(prompt, Vec::new(), None)
                .await?;
            self.application.wait_for_idle().await;
            loop {
                match events.try_recv() {
                    Ok(crate::ApplicationEvent::RunFailed { message }) => {
                        return Err(anyhow!(message));
                    }
                    Ok(_) | Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
                    Err(tokio::sync::broadcast::error::TryRecvError::Closed) => {
                        return Err(anyhow!("workflow supervisor application stopped"));
                    }
                }
            }
            let outcome = if plan_committed.load(Ordering::Acquire) {
                PlanningTurnOutcome::PlanCommitted
            } else if budget_reached.load(Ordering::Acquire) {
                PlanningTurnOutcome::PlanBudgetReached {
                    reason: format!(
                        "planning exceeded the bound ({} assistant turns / {} tool calls)",
                        turns.load(Ordering::Relaxed),
                        tool_calls.load(Ordering::Relaxed),
                    ),
                }
            } else {
                PlanningTurnOutcome::Completed
            };
            Ok(outcome)
        }
        .await;
        // `_hook_guard` clears the stop hook here on every completion path
        // (Ok and Err) and on drop/cancellation.
        outcome
    }

    async fn steer_supervisor(&self, message: String) -> Result<()> {
        self.application.steer(message, Vec::new()).await;
        Ok(())
    }

    fn planning_deadline(&self) -> Duration {
        let ms = self.planning_deadline_ms;
        if ms == 0 {
            Duration::from_secs(90)
        } else {
            Duration::from_millis(ms)
        }
    }

    fn apply_todo(&self, op: TodoOp) -> Result<TodoApplyResult> {
        self.application.apply_todo(op)
    }

    fn reconcile_todo_dag(&self) -> Result<TodoDagExecutionOutcome> {
        self.application.reconcile_todo_dag_if_armed()
    }
    async fn pause(&self) -> Result<()> {
        let ids = self.active_scoped_job_ids();
        self.application.abort().await;
        self.application.wait_for_idle().await;
        self.drain_jobs(&ids).await
    }


    fn execute_todo_dag(&self) -> Result<TodoDagExecutionOutcome> {
        self.application.execute_todo_dag()
    }

    /// P0-C: validate the objective's explicit agent references against the
    /// workflow child catalog before planning spends a turn. Missing or
    /// disabled explicit agents fail actionably instead of silently routing
    /// to the default `task` agent.
    fn validate_objective_agents(&self, objective: &str) -> Result<()> {
        self.orchestration.validate_delegation_agents(objective)
    }

    async fn resume(&self) -> Result<TodoDagExecutionOutcome> {
        self.application.execute_todo_dag()
    }

    async fn cancel_jobs(&self, ids: &[String]) -> Result<Vec<String>> {
        let cancelled = self.orchestration.cancel_jobs(ids);
        self.drain_jobs(&cancelled).await?;
        Ok(cancelled)
    }
}

pub(super) type SharedWorkflowApplicationBackend = Arc<WorkflowApplicationBackend>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    use crate::SessionOptions;
    use parking_lot::Mutex;
    use pi_ai::{
        AssistantMessage, AssistantMessageEvent, ContentBlock, Model, SimpleStreamOptions,
        StopReason, ToolCall, new_assistant_message_event_stream,
    };
    use tokio::sync::Notify;
    use tokio_util::sync::CancellationToken;

    /// Scripted provider stream for the stop-hook tests: pops canned
    /// assistant messages, and when the queue is empty BLOCKS the provider
    /// call until `release` fires (an abort settles the stalled call). The
    /// block is observable (`wait_blocked`) so tests can drop the planning
    /// future while the run is verifiably in flight.
    #[derive(Clone)]
    struct ScriptedStream {
        queue: Arc<Mutex<VecDeque<AssistantMessage>>>,
        blocked: Arc<Notify>,
        blocked_flag: Arc<AtomicBool>,
        release: CancellationToken,
    }

    impl ScriptedStream {
        fn new() -> Self {
            Self {
                queue: Arc::new(Mutex::new(VecDeque::new())),
                blocked: Arc::new(Notify::new()),
                blocked_flag: Arc::new(AtomicBool::new(false)),
                release: CancellationToken::new(),
            }
        }

        fn push(&self, message: AssistantMessage) {
            self.queue.lock().push_back(message);
        }

        async fn wait_blocked(&self) {
            loop {
                let notified = self.blocked.notified();
                if self.blocked_flag.load(Ordering::Acquire) {
                    break;
                }
                notified.await;
            }
        }

        fn stream_fn(&self) -> pi_agent::StreamFn {
            let queue = self.queue.clone();
            let blocked = self.blocked.clone();
            let blocked_flag = self.blocked_flag.clone();
            let release = self.release.clone();
            std::sync::Arc::new(
                move |model: Model, _context: pi_ai::Context, options: SimpleStreamOptions| {
                    let queue = queue.clone();
                    let blocked = blocked.clone();
                    let blocked_flag = blocked_flag.clone();
                    let release = release.clone();
                    Box::pin(async move {
                        let message = queue.lock().pop_front();
                        let message = match message {
                            Some(message) => message,
                            None => {
                                blocked_flag.store(true, Ordering::Release);
                                blocked.notify_one();
                                let abort = options.stream.abort_signal;
                                let mut message = AssistantMessage::pending(&model);
                                tokio::select! {
                                    () = release.cancelled() => {}
                                    () = async {
                                        match abort {
                                            Some(token) => token.cancelled().await,
                                            None => std::future::pending::<()>().await,
                                        }
                                    } => {
                                        message.stop_reason = StopReason::Aborted;
                                        message.error_message =
                                            Some("request aborted".to_owned());
                                        return stream_message(model, message).await;
                                    }
                                }
                                queue.lock().pop_front().unwrap_or_else(|| {
                                    let mut message = AssistantMessage::pending(&model);
                                    message.content = vec![ContentBlock::text("done")];
                                    message.stop_reason = StopReason::Stop;
                                    message
                                })
                            }
                        };
                        stream_message(model, message).await
                    })
                },
            )
        }
    }

    async fn stream_message(
        model: Model,
        message: AssistantMessage,
    ) -> pi_ai::AssistantMessageEventStream {
        let stream = new_assistant_message_event_stream();
        let producer = stream.clone();
        let model = model.clone();
        tokio::spawn(async move {
            producer
                .push(AssistantMessageEvent::Start {
                    partial: AssistantMessage::pending(&model),
                })
                .await;
            let terminal = if matches!(
                message.stop_reason,
                StopReason::Error | StopReason::Aborted
            ) {
                AssistantMessageEvent::Error {
                    reason: message.stop_reason,
                    error: message.clone(),
                }
            } else {
                AssistantMessageEvent::Done {
                    reason: message.stop_reason,
                    message: message.clone(),
                }
            };
            producer.push(terminal).await;
            producer.end(Some(message)).await;
        });
        stream
    }

    fn tool_call_message(name: &str, arguments: serde_json::Value) -> AssistantMessage {
        let mut message = AssistantMessage::pending(&Model::default());
        message.content = vec![ContentBlock::ToolCall(ToolCall {
            id: format!("call-{name}"),
            name: name.to_owned(),
            arguments,
            thought_signature: None,
        })];
        message.stop_reason = StopReason::ToolUse;
        message
    }

    fn text_message(text: &str) -> AssistantMessage {
        let mut message = AssistantMessage::pending(&Model::default());
        message.content = vec![ContentBlock::text(text)];
        message.stop_reason = StopReason::Stop;
        message
    }

    /// Real Application (with the todo tool and a scripted provider) plus a
    /// backend pinned to it and a dummy orchestration. The temp dirs are
    /// returned so cwd/artifact roots stay alive for the test.
    async fn test_backend(
        stream: pi_agent::StreamFn,
    ) -> (
        Arc<WorkflowApplicationBackend>,
        Application,
        tempfile::TempDir,
        tempfile::TempDir,
    ) {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = Session::new_with_todo(SessionOptions {
            model: Model::default(),
            cwd: cwd.path().to_path_buf(),
            system_prompt: String::new(),
            thinking_level: pi_agent::ThinkingLevel::Off,
            api_key: "faux".to_owned(),
            compaction: None,
            stream_options: Default::default(),
            tools: None,
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: Some(stream),
            auth_resolver: None,
        })
        .expect("session");
        let artifacts = tempfile::tempdir().expect("artifacts");
        let definition = crate::parse_agent_definition(
            std::path::Path::new("task.md"),
            "---\nname: task\ndescription: task\n---\nprompt",
            crate::AgentDefinitionSource::Bundled,
            true,
        )
        .expect("definition");
        let config = crate::OrchestrationConfig::new(
            crate::AgentCatalog::from_agents(vec![definition]),
            artifacts.path(),
        );
        let orchestration = crate::OrchestrationRuntime::new(
            config,
            std::sync::Arc::new(|_| Box::pin(async { unreachable!() })),
        )
        .expect("orchestration");
        // Mirror the supervisor's `configure_workflow_runtime`: a workflow
        // scope suppresses the application's Todo auto-arm (BUG-1), so the
        // planning run's `todo init` never spawns worker jobs through the
        // dummy factory.
        orchestration
            .set_workflow_scope(crate::WorkflowRuntimeScope {
                workflow_id: "wf-hook".to_owned(),
                generation: 1,
            })
            .expect("workflow scope");
        let application =
            crate::Application::new_with_orchestration(session, orchestration.clone()).await;
        let backend = WorkflowApplicationBackend::new(
            application.clone(),
            orchestration,
            "wf-hook".to_owned(),
            1,
            0,
        )
        .expect("backend");
        (Arc::new(backend), application, cwd, artifacts)
    }

    fn assistant_tool_turn_count(application: &Application) -> usize {
        application
            .session()
            .history()
            .iter()
            .filter(|message| {
                matches!(message, pi_ai::Message::Assistant(assistant)
                    if assistant
                        .content
                        .iter()
                        .any(|block| matches!(block, ContentBlock::ToolCall(_))))
            })
            .count()
    }

    /// F1 drop path: `await_planning_turn` DROPS the pinned planning future
    /// when the wall-clock deadline or semantic non-progress detection aborts
    /// the turn. The per-turn stop hook installed by `prompt_supervisor` must
    /// be cleared by that drop — a stale hook would silently cap a later
    /// steer run at the planning budget (8 tool-calling turns).
    #[tokio::test]
    async fn dropped_planning_future_clears_stop_hook_before_later_steer() {
        let stream = ScriptedStream::new();
        let (backend, application, _cwd, _artifacts) = test_backend(stream.stream_fn()).await;
        let handle = tokio::spawn({
            let backend = backend.clone();
            async move { backend.prompt_supervisor("plan the work".to_owned()).await }
        });
        // The planning run started and its first provider call blocks; the
        // stop hook is installed on the session.
        tokio::time::timeout(Duration::from_secs(10), stream.wait_blocked())
            .await
            .expect("planning run must reach the provider");
        // Simulate the supervisor's drop of the planning future (deadline /
        // non-progress) while the run is still in flight.
        handle.abort();
        let _ = handle.await;
        // Settle the orphaned session run exactly like the supervisor's
        // pause() before steering.
        application.abort().await;
        application.wait_for_idle().await;

        // A steer run with 12 tool-calling turns must complete every scripted
        // turn: with a stale planning hook it would stop at the 8th. The
        // supervisor's `steer_supervisor` enqueues the message; the session
        // run loop (`continue_run`) then executes it.
        stream.release.cancel();
        for _ in 0..12 {
            stream.push(tool_call_message("todo", serde_json::json!({ "op": "view" })));
        }
        stream.push(text_message("steer done"));
        application.steer("continue the work".to_owned(), Vec::new()).await;
        tokio::time::timeout(Duration::from_secs(10), application.session().continue_run())
            .await
            .expect("steer run must settle")
            .expect("steer run must not error");
        assert_eq!(
            assistant_tool_turn_count(&application),
            12,
            "the steer run must complete every scripted tool turn; a stale planning stop hook would cap it at 8"
        );
        application.cleanup().await;
    }

    /// F1 normal path: a planning prompt that finishes naturally must also
    /// leave no stop hook behind (the guard clears it on the completion
    /// path), so a later steer with tool turns is never capped.
    #[tokio::test]
    async fn completed_planning_prompt_clears_stop_hook_before_later_steer() {
        let stream = ScriptedStream::new();
        let (backend, application, _cwd, _artifacts) = test_backend(stream.stream_fn()).await;
        stream.push(tool_call_message(
            "todo",
            serde_json::json!({ "op": "init", "items": ["ship it"] }),
        ));
        stream.push(text_message("plan committed"));
        let outcome = backend
            .prompt_supervisor("plan the work".to_owned())
            .await
            .expect("planning must complete naturally");
        assert_eq!(outcome, PlanningTurnOutcome::PlanCommitted);
        application.wait_for_idle().await;

        stream.release.cancel();
        for _ in 0..12 {
            stream.push(tool_call_message("todo", serde_json::json!({ "op": "view" })));
        }
        stream.push(text_message("steer done"));
        application.steer("continue the work".to_owned(), Vec::new()).await;
        tokio::time::timeout(Duration::from_secs(10), application.session().continue_run())
            .await
            .expect("steer run must settle")
            .expect("steer run must not error");
        // 1 planning `todo init` turn + 12 steer `todo view` turns. A stale
        // planning hook would cap the steer at 8 tool-calling turns.
        assert_eq!(
            assistant_tool_turn_count(&application),
            13,
            "a completed planning prompt must leave no stop hook behind"
        );
        application.cleanup().await;
    }
}
