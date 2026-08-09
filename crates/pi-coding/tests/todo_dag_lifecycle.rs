//! Deterministic integration coverage for the todo tool DAG lifecycle.
//!
//! Scripts add/update/dependency/readiness/complete/remove through the public
//! todo tool, then defends model-visible readiness fields and on-disk
//! `todo_snapshot` restore. Does not reimplement the graph engine.

use std::path::Path;
use std::sync::Arc;

use pi_agent::{
    AbortController, AgentTool, AgentToolResult, ThinkingLevel, ToolCallContext, ToolUpdateFn,
};
use pi_ai::{ContentBlock, Model, SimpleStreamOptions};
use pi_coding::{
    ResourceDiscovery, Session, SessionOptions, SessionRecorder, TodoItem, TodoPhase, TodoState,
    TodoStatus, TodoStorage, ToolSelection, load_session_tree, resume_session, start_session_in,
};
use serde_json::{Value, json};

/// Wire marker on typed todo domain errors (matches crate-private `TODO_ERROR_MARKER`).
const TODO_ERROR_MARKER: &str = "__piTodoError";

fn noop_update() -> ToolUpdateFn {
    Arc::new(|_result: AgentToolResult| {})
}

fn make_ctx(arguments: Value) -> ToolCallContext {
    let (controller, abort) = AbortController::new();
    std::mem::forget(controller);
    ToolCallContext {
        tool_call_id: "todo-dag-lifecycle".to_owned(),
        arguments,
        on_update: noop_update(),
        abort,
        model: None,
    }
}

fn text_of(result: &AgentToolResult) -> String {
    match result.content.first() {
        Some(ContentBlock::Text { text, .. }) => text.clone(),
        _ => String::new(),
    }
}

async fn call_todo(tool: &AgentTool, arguments: Value) -> AgentToolResult {
    (tool.execute)(make_ctx(arguments))
        .await
        .unwrap_or_else(|error| panic!("todo tool transport error: {error:#}"))
}

fn assert_no_todo_error(result: &AgentToolResult, op: &str) {
    assert_ne!(
        result.details.get(TODO_ERROR_MARKER),
        Some(&Value::Bool(true)),
        "{op} unexpectedly marked todo domain error: {} / {}",
        text_of(result),
        result.details
    );
}

fn task_by_content<'a>(state: &'a TodoState, content: &str) -> &'a TodoItem {
    state
        .phases
        .iter()
        .flat_map(|phase| &phase.tasks)
        .find(|task| task.content == content)
        .unwrap_or_else(|| panic!("missing task {content:?} in {state:?}"))
}

fn task_id(details: &Value, content: &str) -> String {
    details["phases"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|phase| phase["tasks"].as_array().into_iter().flatten())
        .find(|task| task["content"] == content)
        .and_then(|task| task["id"].as_str())
        .unwrap_or_else(|| panic!("missing task id for {content} in {details}"))
        .to_owned()
}

fn session_options(cwd: &Path) -> SessionOptions {
    SessionOptions {
        model: Model {
            id: "todo-dag-lifecycle".to_owned(),
            name: "Todo DAG Lifecycle".to_owned(),
            api: "todo-dag-lifecycle-api".to_owned(),
            provider: "todo-dag-lifecycle-provider".to_owned(),
            ..Model::default()
        },
        cwd: cwd.to_path_buf(),
        system_prompt: String::new(),
        thinking_level: ThinkingLevel::Off,
        api_key: "faux".to_owned(),
        compaction: None,
        stream_options: SimpleStreamOptions::default(),
        tools: None,
        before_tool_call: None,
        after_tool_call: None,
        stream_fn: None,
        auth_resolver: None,
    }
}

fn todo_session(cwd: &Path) -> Session {
    Session::new_with_todo_and_additional_tools_filtered_and_discovery(
        session_options(cwd),
        Vec::new(),
        ToolSelection::default(),
        ResourceDiscovery::Disabled,
    )
    .expect("todo session")
}

/// Isolated session file under a temp agent dir (never touches real ~/.pi).
fn start_isolated_recording(cwd: &Path, session_id: &str) -> (tempfile::TempDir, SessionRecorder) {
    let agent = tempfile::tempdir().expect("agent home");
    let sessions = agent.path().join("sessions");
    std::fs::create_dir_all(&sessions).expect("sessions dir");
    let recorder = start_session_in(
        cwd,
        None,
        Some("off"),
        Some(&sessions),
        Some(session_id),
        None,
    )
    .expect("start isolated recorder");
    (agent, recorder)
}

fn attach_recorder(session: &Session, recorder: SessionRecorder) {
    session.record(recorder).expect("attach recorder");
}

/// Full tool campaign: init/append, dependencies, readiness, complete, remove.
/// Asserts model-visible ready/blockedBy and no silent dependency mutation.
#[tokio::test]
async fn todo_tool_campaign_exercises_dag_lifecycle_and_model_visible_readiness() {
    let cwd = tempfile::tempdir().expect("cwd");
    let session = todo_session(cwd.path());
    let (_agent, recorder) = start_isolated_recording(cwd.path(), "todo-dag-campaign");
    attach_recorder(&session, recorder);
    let todo = session
        .get_tool_definition("todo")
        .expect("todo tool must be enabled");

    // add — init a two-phase graph skeleton.
    let init = call_todo(
        &todo,
        json!({
            "op": "init",
            "list": [
                { "phase": "Build", "items": ["compile", "link"] },
                { "phase": "Verify", "items": ["test"] }
            ]
        }),
    )
    .await;
    assert_no_todo_error(&init, "init");
    assert_eq!(init.details["op"], "init");
    assert_eq!(init.details["storage"], "session");

    let compile_id = task_id(&init.details, "compile");
    let link_id = task_id(&init.details, "link");
    let test_id = task_id(&init.details, "test");
    assert!(compile_id.starts_with("task-"));
    assert_ne!(compile_id, link_id);
    assert_ne!(link_id, test_id);

    // Roots are ready; dependents without edges are also ready until wired.
    assert_eq!(
        init.details["phases"][0]["tasks"][0]["ready"],
        true,
        "compile should start ready"
    );
    assert_eq!(
        init.details["phases"][0]["tasks"][0]["blockedBy"],
        json!([])
    );

    // update — append keeps existing ids stable and does not rewrite dependsOn.
    let append = call_todo(
        &todo,
        json!({
            "op": "append",
            "phase": "Verify",
            "items": ["lint"]
        }),
    )
    .await;
    assert_no_todo_error(&append, "append");
    let lint_id = task_id(&append.details, "lint");
    assert_eq!(task_id(&append.details, "compile"), compile_id);
    assert_eq!(task_id(&append.details, "link"), link_id);
    assert_eq!(task_id(&append.details, "test"), test_id);
    assert_eq!(
        append.details["phases"][0]["tasks"][0]["dependsOn"],
        json!([]),
        "append must not silently invent dependencies on existing tasks"
    );

    // dependency — wire a chain with explicit ids only.
    let add_link_dep = call_todo(
        &todo,
        json!({
            "op": "add_dependency",
            "task": link_id,
            "dependsOn": [compile_id]
        }),
    )
    .await;
    assert_no_todo_error(&add_link_dep, "add_dependency link");
    let add_test_dep = call_todo(
        &todo,
        json!({
            "op": "add_dependency",
            "task": test_id,
            "dependsOn": [link_id]
        }),
    )
    .await;
    assert_no_todo_error(&add_test_dep, "add_dependency test");
    let add_lint_dep = call_todo(
        &todo,
        json!({
            "op": "update_dependencies",
            "task": lint_id,
            "dependsOn": [compile_id, test_id]
        }),
    )
    .await;
    assert_no_todo_error(&add_lint_dep, "update_dependencies lint");

    let after_deps = session.todo_state();
    let compile = task_by_content(&after_deps, "compile");
    let link = task_by_content(&after_deps, "link");
    let test = task_by_content(&after_deps, "test");
    let lint = task_by_content(&after_deps, "lint");

    assert!(compile.ready, "root compile must be model-visible ready");
    assert!(compile.blocked_by.is_empty());
    assert!(!link.ready, "link must wait on compile");
    assert_eq!(link.depends_on, vec![compile_id.clone()]);
    assert_eq!(link.blocked_by.len(), 1);
    assert_eq!(link.blocked_by[0].task_id, compile_id);
    assert!(!test.ready, "test must wait on link");
    assert_eq!(test.depends_on, vec![link_id.clone()]);
    assert!(!lint.ready, "lint must wait on compile and test");
    assert_eq!(lint.depends_on, vec![compile_id.clone(), test_id.clone()]);

    // View is read-only and must echo the same readiness projection.
    let view = call_todo(&todo, json!({ "op": "view" })).await;
    assert_no_todo_error(&view, "view");
    assert_eq!(view.details["op"], "view");
    assert_eq!(
        view.details["phases"][0]["tasks"][1]["ready"],
        false,
        "model-visible view must keep link blocked"
    );
    assert_eq!(
        view.details["phases"][0]["tasks"][1]["blockedBy"][0]["taskId"],
        compile_id
    );
    assert_eq!(
        session.todo_state(),
        after_deps,
        "view must not mutate canonical state"
    );

    // Reject cycles without mutating edges.
    let cycle = call_todo(
        &todo,
        json!({
            "op": "add_dependency",
            "task": compile_id,
            "dependsOn": [test_id]
        }),
    )
    .await;
    assert_eq!(cycle.details[TODO_ERROR_MARKER], true);
    assert!(
        text_of(&cycle).contains("cycle"),
        "cycle rejection text missing: {}",
        text_of(&cycle)
    );
    assert_eq!(
        session.todo_state().phases[0].tasks[0].depends_on,
        Vec::<String>::new(),
        "failed cycle must not silently attach reverse edges"
    );

    // readiness unlock — completing compile unblocks link only.
    let start = call_todo(
        &todo,
        json!({
            "op": "start",
            "task": compile_id
        }),
    )
    .await;
    assert_no_todo_error(&start, "start compile");
    let done_compile = call_todo(
        &todo,
        json!({
            "op": "done",
            "task": compile_id
        }),
    )
    .await;
    assert_no_todo_error(&done_compile, "done compile");
    assert_eq!(
        done_compile.details["completedTasks"][0]["content"],
        "compile"
    );

    let unlocked = session.todo_state();
    let link = task_by_content(&unlocked, "link");
    let test = task_by_content(&unlocked, "test");
    let lint = task_by_content(&unlocked, "lint");
    assert_eq!(
        task_by_content(&unlocked, "compile").status,
        TodoStatus::Completed
    );
    assert!(link.ready, "link must become ready after compile completes");
    assert!(link.blocked_by.is_empty());
    assert_eq!(
        link.depends_on,
        vec![compile_id.clone()],
        "completion must not silently drop dependsOn edges"
    );
    assert!(!test.ready, "test still blocked on incomplete link");
    assert!(!lint.ready, "lint still blocked on incomplete test");
    assert_eq!(lint.depends_on, vec![compile_id.clone(), test_id.clone()]);

    // complete the rest of the chain.
    let done_link = call_todo(&todo, json!({ "op": "done", "task": link_id })).await;
    assert_no_todo_error(&done_link, "done link");
    let done_test = call_todo(&todo, json!({ "op": "done", "task": test_id })).await;
    assert_no_todo_error(&done_test, "done test");
    let after_test = session.todo_state();
    assert!(
        task_by_content(&after_test, "lint").ready,
        "lint must unlock once compile+test are terminal"
    );
    assert!(task_by_content(&after_test, "lint").blocked_by.is_empty());

    // remove_dependency is explicit — dropping only the requested edge.
    let remove_one = call_todo(
        &todo,
        json!({
            "op": "remove_dependency",
            "task": lint_id,
            "dependsOn": [test_id]
        }),
    )
    .await;
    assert_no_todo_error(&remove_one, "remove_dependency");
    assert_eq!(
        task_by_content(&session.todo_state(), "lint").depends_on,
        vec![compile_id.clone()],
        "remove_dependency must keep the other explicit edge"
    );

    // remove — cascade delete of compile must require cascade.
    let rm_blocked = call_todo(
        &todo,
        json!({
            "op": "rm",
            "task": compile_id,
            "cascade": false
        }),
    )
    .await;
    assert_eq!(rm_blocked.details[TODO_ERROR_MARKER], true);
    assert!(
        text_of(&rm_blocked).contains("Cannot remove dependency target")
            || text_of(&rm_blocked).contains("cascade=true"),
        "rm without cascade must reject dependency targets: {}",
        text_of(&rm_blocked)
    );
    assert!(
        session
            .todo_state()
            .phases
            .iter()
            .flat_map(|phase| &phase.tasks)
            .any(|task| task.id == compile_id),
        "rejected rm must leave compile in place"
    );

    let rm_lint = call_todo(
        &todo,
        json!({
            "op": "rm",
            "task": lint_id,
            "cascade": false
        }),
    )
    .await;
    assert_no_todo_error(&rm_lint, "rm lint");
    assert!(
        session
            .todo_state()
            .phases
            .iter()
            .flat_map(|phase| &phase.tasks)
            .all(|task| task.id != lint_id),
        "lint must be removed from public state"
    );
    // Remaining tasks must keep their explicit dependency lists intact.
    assert_eq!(
        task_by_content(&session.todo_state(), "link").depends_on,
        vec![compile_id.clone()]
    );
    assert_eq!(
        task_by_content(&session.todo_state(), "test").depends_on,
        vec![link_id.clone()]
    );

    // Public session snapshot must match the tool-visible terminal graph.
    let final_state = session.todo_state();
    assert_eq!(final_state.storage, TodoStorage::Session);
    assert_eq!(final_state.phases.len(), 2);
    assert_eq!(
        final_state
            .phases
            .iter()
            .flat_map(|phase| phase.tasks.iter().map(|task| task.content.as_str()))
            .collect::<Vec<_>>(),
        vec!["compile", "link", "test"]
    );
}

/// Todo tool mutations persist `todo_snapshot` records; resume restores readiness.
#[tokio::test]
async fn todo_tool_persisted_snapshot_restores_readiness_without_silent_edge_rewrites() {
    let cwd = tempfile::tempdir().expect("cwd");
    let session = todo_session(cwd.path());
    let (_agent, recorder) = start_isolated_recording(cwd.path(), "todo-dag-restore");
    let path = recorder.path();
    attach_recorder(&session, recorder.clone());
    let todo = session
        .get_tool_definition("todo")
        .expect("todo tool");

    let init = call_todo(
        &todo,
        json!({
            "op": "init",
            "list": [{ "phase": "Ship", "items": ["design", "implement", "review"] }]
        }),
    )
    .await;
    assert_no_todo_error(&init, "init");
    let design_id = task_id(&init.details, "design");
    let implement_id = task_id(&init.details, "implement");
    let review_id = task_id(&init.details, "review");

    let wired = call_todo(
        &todo,
        json!({
            "op": "update_dependencies",
            "task": implement_id,
            "dependsOn": [design_id]
        }),
    )
    .await;
    assert_no_todo_error(&wired, "wire implement");
    let wired_review = call_todo(
        &todo,
        json!({
            "op": "add_dependency",
            "task": review_id,
            "dependsOn": [implement_id]
        }),
    )
    .await;
    assert_no_todo_error(&wired_review, "wire review");
    let done_design = call_todo(&todo, json!({ "op": "done", "task": design_id })).await;
    assert_no_todo_error(&done_design, "done design");

    let live = session.todo_state();
    assert_eq!(
        task_by_content(&live, "design").status,
        TodoStatus::Completed
    );
    assert!(task_by_content(&live, "implement").ready);
    assert_eq!(
        task_by_content(&live, "implement").depends_on,
        vec![design_id.clone()]
    );
    assert!(!task_by_content(&live, "review").ready);
    assert_eq!(
        task_by_content(&live, "review").depends_on,
        vec![implement_id.clone()]
    );

    // Every successful mutation is durable before the tool call returns. The
    // session file therefore exists without an assistant message, timer, or
    // explicit flush, and a clean close has no Todo-only tail left to rescue.
    assert!(
        path.is_file(),
        "todo mutation must write through to disk immediately: {}",
        path.display()
    );
    assert_eq!(
        load_session_tree(&path)
            .expect("load write-through tree")
            .latest_todo_state(),
        Some(live.clone())
    );
    // Drop the live session and close its writer before reopening, matching a
    // normal rpi close followed by --resume.
    drop(session);
    recorder.close().expect("close writer before reopen");

    let tree = load_session_tree(&path).expect("load tree");
    let snapshot = tree
        .latest_todo_state()
        .expect("persisted todo_snapshot state");
    assert_eq!(snapshot.storage, TodoStorage::Session);
    assert_eq!(task_by_content(&snapshot, "design").id, design_id);
    assert_eq!(task_by_content(&snapshot, "implement").id, implement_id);
    assert_eq!(
        task_by_content(&snapshot, "implement").depends_on,
        vec![design_id.clone()],
        "snapshot must preserve explicit dependsOn bytes"
    );
    assert_eq!(
        task_by_content(&snapshot, "review").depends_on,
        vec![implement_id.clone()]
    );
    // Projection fields are part of the public snapshot contract.
    assert!(task_by_content(&snapshot, "implement").ready);
    assert!(task_by_content(&snapshot, "implement").blocked_by.is_empty());
    assert!(!task_by_content(&snapshot, "review").ready);
    assert_eq!(
        task_by_content(&snapshot, "review").blocked_by[0].task_id,
        implement_id
    );

    let restored = todo_session(cwd.path());
    let restored_recorder = resume_session(&path).expect("resume recorder");
    restored
        .record(restored_recorder.clone())
        .expect("attach recorder");
    let restored_state = restored.todo_state();
    assert_eq!(
        restored_state, snapshot,
        "session restore must surface the public TodoSnapshot state"
    );
    assert_eq!(
        task_by_content(&restored_state, "implement").depends_on,
        vec![design_id.clone()],
        "restore must not silently rewrite dependency edges"
    );
    assert!(
        task_by_content(&restored_state, "implement").ready,
        "restored implement readiness must stay model-visible"
    );
    assert!(
        !task_by_content(&restored_state, "review").ready,
        "restored review must remain blocked"
    );

    // Further tool ops on the restored session keep persistence coherent.
    let todo = restored
        .get_tool_definition("todo")
        .expect("todo after restore");
    let done_implement = call_todo(
        &todo,
        json!({ "op": "done", "task": implement_id }),
    )
    .await;
    assert_no_todo_error(&done_implement, "done implement after restore");
    let after = restored.todo_state();
    assert_eq!(
        task_by_content(&after, "implement").status,
        TodoStatus::Completed
    );
    assert!(
        task_by_content(&after, "review").ready,
        "completing restored implement must unlock review"
    );
    assert_eq!(
        task_by_content(&after, "review").depends_on,
        vec![implement_id.clone()],
        "unlock must not strip dependsOn"
    );

    // The restored mutation is also write-through; no explicit final flush is
    // required for the next process to observe it.
    let reloaded = load_session_tree(&path)
        .expect("reload tree")
        .latest_todo_state()
        .expect("latest snapshot after mutation");
    assert_eq!(reloaded, after);

    // A different session owns a different Todo journal and starts empty.
    let (_other_agent, other_recorder) =
        start_isolated_recording(cwd.path(), "todo-dag-restore-isolated");
    let other = todo_session(cwd.path());
    attach_recorder(&other, other_recorder);
    assert!(
        other.todo_state().phases.is_empty(),
        "Todo restore must remain isolated to the resumed session"
    );
}

/// Abrupt termination can only restore snapshots whose durable append
/// completed. An operation interrupted before that append is not successful
/// and must leave the previous durable state as the recovery point.
#[test]
fn interrupted_unwritten_todo_operation_restores_last_durable_snapshot() {
    let cwd = tempfile::tempdir().expect("cwd");
    let (_agent, recorder) = start_isolated_recording(cwd.path(), "todo-crash-boundary");
    let durable = TodoState {
        phases: vec![TodoPhase {
            name: "Recovery".to_owned(),
            tasks: vec![TodoItem {
                id: "task-written".to_owned(),
                content: "written before interruption".to_owned(),
                status: TodoStatus::InProgress,
                depends_on: Vec::new(),
                ready: true,
                blocked_by: Vec::new(),
                agent: None,
            }],
        }],
        storage: TodoStorage::Session,
    };
    recorder
        .record_todo_snapshot(&durable)
        .expect("durably record completed operation");
    let path = recorder.path();

    // This is the prospective state of an operation interrupted before its
    // recorder append. Deliberately do not record or close the live recorder,
    // which models process death at that boundary.
    let mut interrupted = durable.clone();
    interrupted.phases[0].tasks.push(TodoItem {
        id: "task-unwritten".to_owned(),
        content: "interrupted before durable append".to_owned(),
        status: TodoStatus::Pending,
        depends_on: Vec::new(),
        ready: false,
        blocked_by: Vec::new(),
        agent: None,
    });
    assert_ne!(interrupted, durable);
    drop(recorder);

    let restored = todo_session(cwd.path());
    restored
        .record(resume_session(&path).expect("resume after interruption"))
        .expect("attach interrupted session");
    assert_eq!(
        restored.todo_state(),
        durable,
        "resume must stop at the last fully written Todo operation"
    );
}

/// Direct recorder snapshot attach path restores complete DAG public state.
#[test]
fn attaching_recorded_todo_snapshot_restores_public_state_bytes() {
    let cwd = tempfile::tempdir().expect("cwd");
    let recorder = start_session_in(
        cwd.path(),
        None,
        Some("off"),
        Some(cwd.path()),
        Some("todo-dag-lifecycle-attach"),
        None,
    )
    .expect("start session");
    let state = TodoState {
        phases: vec![TodoPhase {
            name: "Graph".to_owned(),
            tasks: vec![
                TodoItem {
                    id: "task-root".to_owned(),
                    content: "root".to_owned(),
                    status: TodoStatus::Completed,
                    depends_on: Vec::new(),
                    ready: false,
                    blocked_by: Vec::new(),
                    agent: None,
                },
                TodoItem {
                    id: "task-child".to_owned(),
                    content: "child".to_owned(),
                    status: TodoStatus::InProgress,
                    depends_on: vec!["task-root".to_owned()],
                    ready: true,
                    blocked_by: Vec::new(),
                    agent: None,
                },
            ],
        }],
        storage: TodoStorage::Session,
    };
    recorder
        .record_todo_snapshot(&state)
        .expect("record todo snapshot");
    recorder.persist_now().expect("persist session");
    let path = recorder.path();
    recorder.close().expect("close");

    let session = todo_session(cwd.path());
    session
        .record(resume_session(&path).expect("resume"))
        .expect("attach");
    assert_eq!(session.todo_state(), state);

    let tree_state = load_session_tree(&path)
        .expect("tree")
        .latest_todo_state()
        .expect("snapshot");
    assert_eq!(tree_state, state);
    assert_eq!(tree_state.phases[0].tasks[1].depends_on, ["task-root"]);
    assert!(tree_state.phases[0].tasks[1].ready);
}
