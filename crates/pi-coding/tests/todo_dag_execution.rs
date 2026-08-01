use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use parking_lot::Mutex;
use pi_agent::ThinkingLevel;
use pi_ai::{ContentBlock, Model, StopReason};
use pi_coding::{
    AgentCatalog, AgentDefinition, AgentDefinitionSource, Application, ChildSessionFactory,
    JobStatus, OrchestrationConfig, OrchestrationRuntime, Session, SessionOptions,
    TodoDagExecutionStatus, TodoItem, TodoPhase, TodoStatus,
};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

fn definition() -> AgentDefinition {
    AgentDefinition {
        name: "task".to_owned(), description: "execute a Todo DAG item".to_owned(),
        system_prompt: "complete the assigned Todo item".to_owned(), tools: Some(Vec::new()),
        autoload_skills: Vec::new(), model: None, thinking_level: Some(ThinkingLevel::Off),
        source: AgentDefinitionSource::Bundled, path: None, trusted: true,
    }
}

fn parent_session(cwd: &std::path::Path, model: Model) -> Session {
    Session::new(SessionOptions { model, cwd: cwd.to_path_buf(), system_prompt: String::new(),
        thinking_level: ThinkingLevel::Off, api_key: String::new(), compaction: None,
        stream_options: Default::default(), tools: Some(Vec::new()), before_tool_call: None,
        after_tool_call: None, stream_fn: None, auth_resolver: None }).expect("parent session")
}

struct ControlledDag {
    runtime: OrchestrationRuntime, started: Arc<Mutex<HashSet<String>>>, started_changed: Arc<Notify>,
    releases: Arc<Mutex<HashMap<String, CancellationToken>>>, current: Arc<AtomicUsize>, peak: Arc<AtomicUsize>,
}

impl ControlledDag {
    async fn wait_started(&self, id: &str) {
        tokio::time::timeout(Duration::from_secs(2), async { loop { let changed = self.started_changed.notified(); if self.started.lock().contains(id) { break; } changed.await; } })
            .await.unwrap_or_else(|_| panic!("timed out waiting for {id} to start"));
    }
    fn release(&self, id: &str) { self.releases.lock().get(id).unwrap_or_else(|| panic!("missing release gate for {id}")).cancel(); }
}

fn controlled_dag(artifact_dir: &std::path::Path, max_concurrency: usize, failing_ids: HashSet<String>) -> ControlledDag {
    let started = Arc::new(Mutex::new(HashSet::new())); let started_changed = Arc::new(Notify::new());
    let releases = Arc::new(Mutex::new(HashMap::<String, CancellationToken>::new()));
    let current = Arc::new(AtomicUsize::new(0)); let peak = Arc::new(AtomicUsize::new(0));
    let factory_started = started.clone(); let factory_started_changed = started_changed.clone();
    let factory_releases = releases.clone(); let factory_current = current.clone(); let factory_peak = peak.clone();
    let factory: ChildSessionFactory = Arc::new(move |request| {
        let started = factory_started.clone(); let started_changed = factory_started_changed.clone();
        let releases = factory_releases.clone(); let current = factory_current.clone(); let peak = factory_peak.clone();
        let failing = failing_ids.contains(&request.child_id);
        Box::pin(async move {
            if failing { anyhow::bail!("planned child failure for {}", request.child_id); }
            let child_id = request.child_id.clone();
            let release = { let mut releases = releases.lock(); releases.entry(child_id.clone()).or_insert_with(CancellationToken::new).clone() };
            let stream_fn: pi_agent::StreamFn = Arc::new(move |model, _context, options| {
                let child_id = child_id.clone(); let started = started.clone(); let started_changed = started_changed.clone();
                let release = release.clone(); let current = current.clone(); let peak = peak.clone();
                Box::pin(async move {
                    let stream = pi_ai::new_assistant_message_event_stream(); let producer = stream.clone();
                    tokio::spawn(async move {
                        let active = current.fetch_add(1, Ordering::SeqCst) + 1; peak.fetch_max(active, Ordering::SeqCst);
                        started.lock().insert(child_id); started_changed.notify_waiters();
                        let abort = options.stream.abort_signal.expect("child stream abort signal");
                        let aborted = tokio::select! { () = release.cancelled() => false, () = abort.cancelled() => true };
                        current.fetch_sub(1, Ordering::SeqCst); let mut message = pi_ai::AssistantMessage::pending(&model);
                        if aborted { message.stop_reason = StopReason::Aborted; message.error_message = Some("cancelled by test".to_owned()); }
                        else { message.content.push(ContentBlock::text("completed owned Todo")); message.stop_reason = StopReason::Stop; }
                        producer.end(Some(message)).await;
                    }); stream
                })
            });
            Session::new(SessionOptions { model: request.model, cwd: std::env::current_dir().expect("cwd"),
                system_prompt: request.system_prompt, thinking_level: ThinkingLevel::Off, api_key: String::new(),
                compaction: None, stream_options: Default::default(), tools: Some(request.orchestration_tools),
                before_tool_call: None, after_tool_call: None, stream_fn: Some(stream_fn), auth_resolver: None })
        })
    });
    let model = Model { id: "todo-dag-execution".to_owned(), name: "Todo DAG Execution".to_owned(),
        api: "todo-dag-execution".to_owned(), provider: "todo-dag-execution".to_owned(), ..Model::default() };
    let mut config = OrchestrationConfig::new(AgentCatalog::from_agents(vec![definition()]), artifact_dir);
    config.max_concurrency = max_concurrency; config.idle_ttl = None; config.parent_model = model;
    ControlledDag { runtime: OrchestrationRuntime::new(config, factory).expect("orchestration runtime"),
        started, started_changed, releases, current, peak }
}

fn task(id: &str, content: &str, depends_on: &[&str]) -> TodoItem {
    TodoItem { id: id.to_owned(), content: content.to_owned(), status: TodoStatus::Pending,
        depends_on: depends_on.iter().map(|id| (*id).to_owned()).collect(), ready: false, blocked_by: Vec::new() }
}

async fn wait_todo_status(application: &Application, id: &str, expected: TodoStatus) {
    tokio::time::timeout(Duration::from_secs(2), async { loop {
        let status = application.todo_state().phases.iter().flat_map(|phase| &phase.tasks)
            .find(|task| task.id == id).map(|task| task.status);
        if status == Some(expected) { break; } tokio::task::yield_now().await;
    }}).await.unwrap_or_else(|_| panic!("timed out waiting for Todo {id} to become {expected:?}"));
}

async fn wait_all_settled(runtime: &OrchestrationRuntime, expected: usize) -> Vec<pi_coding::JobSnapshot> {
    tokio::time::timeout(Duration::from_secs(2), async { loop { let jobs = runtime.jobs(None);
        if jobs.len() == expected && jobs.iter().all(|job| job.status.is_settled()) { break jobs; }
        tokio::task::yield_now().await;
    }}).await.expect("jobs did not all settle")
}

#[tokio::test]
async fn two_ready_roots_overlap_and_join_waits_for_both_before_three_of_three() {
    let root = tempfile::tempdir().expect("root"); let controlled = controlled_dag(root.path(), 2, HashSet::new());
    let model = Model { id: "todo-dag-execution".to_owned(), name: "Todo DAG Execution".to_owned(), api: "todo-dag-execution".to_owned(), provider: "todo-dag-execution".to_owned(), ..Model::default() };
    let application = Application::new_with_orchestration(parent_session(root.path(), model), controlled.runtime.clone()).await;
    application.set_todos(vec![TodoPhase { name: "Roots".to_owned(), tasks: vec![task("root-a", "complete root A", &[]), task("root-b", "complete root B", &[])] },
        TodoPhase { name: "Join".to_owned(), tasks: vec![task("join", "complete the join", &["root-a", "root-b"])] }]).expect("set Todo DAG");
    controlled.wait_started("Todo1").await; controlled.wait_started("Todo2").await;
    assert_eq!(controlled.peak.load(Ordering::SeqCst), 2, "ready roots must overlap"); assert_eq!(controlled.current.load(Ordering::SeqCst), 2);
    assert_eq!(controlled.runtime.jobs(None).len(), 2);
    assert!(controlled.runtime.jobs(None).iter().all(|job| matches!(job.todo_task_id.as_deref(), Some("root-a" | "root-b"))));
    assert!(controlled.runtime.jobs(None).iter().all(|job| job.todo_task_id.as_deref() != Some("join")));
    controlled.release("Todo1"); wait_todo_status(&application, "root-a", TodoStatus::Completed).await;
    assert!(controlled.runtime.jobs(None).iter().all(|job| job.todo_task_id.as_deref() != Some("join")), "join must wait for both roots");
    controlled.release("Todo2"); controlled.wait_started("Todo3").await; wait_todo_status(&application, "root-b", TodoStatus::Completed).await;
    let join_job = controlled.runtime.jobs(None).into_iter().find(|job| job.todo_task_id.as_deref() == Some("join")).expect("join owner spawned");
    assert_eq!(join_job.status, JobStatus::Running); controlled.release("Todo3");
    let terminal = tokio::time::timeout(Duration::from_secs(2), application.wait_todo_dag()).await.expect("Todo DAG terminal timeout");
    assert_eq!(terminal, TodoDagExecutionStatus::Settled); let state = application.todo_state();
    assert_eq!(state.phases.iter().flat_map(|phase| &phase.tasks).filter(|task| task.status == TodoStatus::Completed).count(), 3);
    assert!(state.phases.iter().flat_map(|phase| &phase.tasks).all(|task| task.status == TodoStatus::Completed)); application.cleanup().await;
}

#[tokio::test]
async fn failed_and_cancelled_owners_stay_open_and_terminal_reconciliation_is_idempotent() {
    let root = tempfile::tempdir().expect("root"); let controlled = controlled_dag(root.path(), 2, HashSet::from(["Todo1".to_owned()]));
    let model = Model { id: "todo-dag-execution".to_owned(), name: "Todo DAG Execution".to_owned(), api: "todo-dag-execution".to_owned(), provider: "todo-dag-execution".to_owned(), ..Model::default() };
    let application = Application::new_with_orchestration(parent_session(root.path(), model), controlled.runtime.clone()).await;
    application.set_todos(vec![TodoPhase { name: "Failures".to_owned(), tasks: vec![task("fails", "fail this owner", &[]), task("cancels", "cancel this owner", &[]), task("blocked", "remain blocked", &["fails", "cancels"])] }]).expect("set Todo DAG");
    controlled.wait_started("Todo2").await; let initial = controlled.runtime.jobs(None); assert_eq!(initial.len(), 2);
    let cancel_job = initial.iter().find(|job| job.todo_task_id.as_deref() == Some("cancels")).expect("cancel owner");
    assert_eq!(controlled.runtime.cancel_jobs(std::slice::from_ref(&cancel_job.id)), vec![cancel_job.id.clone()]);
    let settled = wait_all_settled(&controlled.runtime, 2).await; assert!(settled.iter().any(|job| job.status == JobStatus::Failed)); assert!(settled.iter().any(|job| job.status == JobStatus::Cancelled));
    let terminal = tokio::time::timeout(Duration::from_secs(2), application.wait_todo_dag()).await.expect("blocked Todo DAG timeout"); assert_eq!(terminal, TodoDagExecutionStatus::Blocked);
    let before = application.todo_state(); assert!(before.phases.iter().flat_map(|phase| &phase.tasks).all(|task| task.status != TodoStatus::Completed)); assert_eq!(controlled.runtime.jobs(None).len(), 2);
    for _ in 0..2 { let outcome = application.reconcile_todo_dag_if_armed().expect("idempotent reconcile"); assert_eq!(outcome.status, TodoDagExecutionStatus::Blocked); assert!(outcome.spawns.is_empty()); }
    assert_eq!(application.todo_state(), before); assert_eq!(controlled.runtime.jobs(None).len(), 2, "terminal jobs must not respawn");
    let retry = application.execute_todo_dag().expect("explicit retry cycle"); assert_eq!(retry.status, TodoDagExecutionStatus::Active); assert_eq!(retry.spawns.len(), 2, "explicit re-arm may retry open failed tasks"); assert_eq!(controlled.runtime.jobs(None).len(), 4);
    application.cleanup().await;
}
