use super::*;
use crate::modes::json::write_json_line;
use serde_json::json;

#[test]
fn command_fixtures_deserialize_and_reject_unknown_fields() {
    let fixtures = [
        json!({"type":"workflow_create","name":"ship","objective":"land multi-workflow"}),
        json!({"type":"workflow_list"}),
        json!({"type":"workflow_get","workflowId":"wf-1"}),
        json!({"type":"workflow_get","name":"ship"}),
        json!({"type":"workflow_pause","workflowId":"wf-1"}),
        json!({"type":"workflow_resume","workflowId":"wf-1"}),
        json!({"type":"workflow_cancel","workflowId":"wf-1"}),
        json!({"type":"workflow_integrate","workflowId":"wf-1"}),
        json!({"type":"workflow_remove","workflowId":"wf-1"}),
    ];
    for fixture in fixtures {
        let command: WorkflowRpcCommand =
            serde_json::from_value(fixture.clone()).expect("fixture must parse");
        assert!(WorkflowRpcCommand::is_workflow_type(command.command_name()));
    }

    let err = serde_json::from_value::<WorkflowRpcCommand>(json!({
        "type":"workflow_create",
        "name":"ship",
        "objective":"x",
        "extraField": true
    }))
    .expect_err("unknown fields must fail");
    assert!(
        err.to_string().contains("unknown field") || err.to_string().contains("extraField"),
        "{err}"
    );
}

#[tokio::test]
async fn create_list_get_pause_shapes_and_redact_worktree() {
    let host = MemoryWorkflowRpcHost::new();
    let created = dispatch_workflow_command(
        &host,
        WorkflowRpcCommand::WorkflowCreate {
            id: Some("c1".into()),
            name: "ship".into(),
            objective: "land foundation".into(),
        },
    )
    .await
    .expect("create");
    assert_eq!(created["name"], "ship");
    assert_eq!(created["status"], "queued");
    assert_eq!(created["generation"], 1);
    assert!(created.get("workflowId").and_then(Value::as_str).is_some());
    let worktree = created["worktree"].as_str().expect("worktree label");
    assert!(
        !Path::new(worktree).is_absolute(),
        "worktree must not be absolute: {worktree}"
    );
    let encoded = serde_json::to_string(&created).unwrap();
    assert!(
        !wire_json_leaks_absolute_path(&encoded),
        "absolute path leaked: {encoded}"
    );

    let workflow_id = created["workflowId"].as_str().unwrap().to_owned();
    let listed = dispatch_workflow_command(
        &host,
        WorkflowRpcCommand::WorkflowList {
            id: Some("l1".into()),
        },
    )
    .await
    .expect("list");
    assert_eq!(listed["workflows"].as_array().unwrap().len(), 1);
    assert_eq!(listed["workflows"][0]["workflowId"], workflow_id);

    let got = dispatch_workflow_command(
        &host,
        WorkflowRpcCommand::WorkflowGet {
            id: None,
            workflow_id: Some(workflow_id.clone()),
            name: None,
        },
    )
    .await
    .expect("get");
    assert_eq!(got["objective"], "land foundation");

    host.resume(&workflow_id).await.expect("resume");
    let paused = dispatch_workflow_command(
        &host,
        WorkflowRpcCommand::WorkflowPause {
            id: Some("p1".into()),
            workflow_id: workflow_id.clone(),
        },
    )
    .await
    .expect("pause");
    assert_eq!(paused["status"], "paused");
    assert!(paused["generation"].as_u64().unwrap() >= 2);
}

#[test]
fn event_serialization_includes_workflow_id_generation_and_redacts_path() {
    let absolute = std::env::temp_dir()
        .join(".pi")
        .join("worktrees")
        .join("wf-event-1");
    assert!(absolute.is_absolute());
    let host_snap = WorkflowSnapshot {
        workflow_id: WorkflowId::new("wf-event-1"),
        name: "alpha".into(),
        objective: "obj".into(),
        status: WorkflowStatus::Running,
        created_at_ms: 1,
        updated_at_ms: 1,
        generation: 7,
        todo: pi_coding::TodoState {
            phases: Vec::new(),
            storage: pi_coding::TodoStorage::Memory,
        },
        worktree_label: Some(absolute.to_string_lossy().into_owned()),
        branch: Some("workflow/wf-event-1".into()),
        supervisor_agent_id: Some("sup-1".into()),
        supervisor_job_id: None,
        failure: None,
        integration: pi_coding::WorkflowIntegration::None,
    };
    let event = project_workflow_event(&WorkflowEvent::Updated {
        snapshot: host_snap,
    });
    let value = serde_json::to_value(&event).unwrap();
    assert_eq!(value["type"], "workflow_updated");
    assert_eq!(value["workflowId"], "wf-event-1");
    assert_eq!(value["generation"], 7);
    assert_eq!(value["snapshot"]["worktree"], "wf-event-1");
    let encoded = serde_json::to_string(&event).unwrap();
    assert!(!wire_json_leaks_absolute_path(&encoded));

    let err = serde_json::from_value::<WorkflowWireEvent>(json!({
        "type": "workflow_updated",
        "workflowId": "wf-1",
        "generation": 1,
        "snapshot": {
            "workflowId": "wf-1",
            "name": "n",
            "objective": "o",
            "status": "queued",
            "generation": 1
        },
        "secretPath": true
    }))
    .expect_err("unknown event field");
    assert!(err.to_string().contains("unknown field"), "{err}");
}

#[test]
fn snapshot_projects_exact_composite_todo_ownership_without_content() {
    let host_snap = WorkflowSnapshot {
        workflow_id: WorkflowId::new("wf-own-1"),
        name: "alpha".into(),
        objective: "obj".into(),
        status: WorkflowStatus::Running,
        created_at_ms: 1,
        updated_at_ms: 1,
        generation: 7,
        todo: pi_coding::TodoState {
            phases: vec![
                pi_coding::TodoPhase {
                    name: "design".into(),
                    tasks: vec![
                        pi_coding::TodoItem {
                            id: "task-a".into(),
                            content: "/private/task-content-a".into(),
                            status: pi_coding::TodoStatus::Pending,
                            depends_on: Vec::new(),
                            ready: false,
                            blocked_by: Vec::new(),
                        },
                        pi_coding::TodoItem {
                            id: "task-b".into(),
                            content: "/private/task-content-b".into(),
                            status: pi_coding::TodoStatus::InProgress,
                            depends_on: Vec::new(),
                            ready: true,
                            blocked_by: Vec::new(),
                        },
                    ],
                },
                pi_coding::TodoPhase {
                    name: "verify".into(),
                    tasks: vec![pi_coding::TodoItem {
                        id: "task-c".into(),
                        content: "/private/task-content-c".into(),
                        status: pi_coding::TodoStatus::Completed,
                        depends_on: Vec::new(),
                        ready: true,
                        blocked_by: Vec::new(),
                    }],
                },
            ],
            storage: pi_coding::TodoStorage::Memory,
        },
        worktree_label: Some("wf-own-1".into()),
        branch: None,
        supervisor_agent_id: None,
        supervisor_job_id: None,
        failure: None,
        integration: pi_coding::WorkflowIntegration::None,
    };

    let projected = project_workflow_snapshot(&host_snap);
    assert_eq!(
        projected.ownership,
        vec![
            pi_coding::WorkflowTaskOwnership {
                workflow_id: "wf-own-1".into(),
                todo_task_id: "task-a".into(),
                generation: 7,
            },
            pi_coding::WorkflowTaskOwnership {
                workflow_id: "wf-own-1".into(),
                todo_task_id: "task-b".into(),
                generation: 7,
            },
            pi_coding::WorkflowTaskOwnership {
                workflow_id: "wf-own-1".into(),
                todo_task_id: "task-c".into(),
                generation: 7,
            },
        ]
    );

    let value = serde_json::to_value(&projected).unwrap();
    let ownership = value["ownership"].as_array().expect("ownership list");
    assert_eq!(ownership.len(), 3);
    for (index, task_id) in ["task-a", "task-b", "task-c"].iter().enumerate() {
        assert_eq!(ownership[index]["workflowId"], "wf-own-1");
        assert_eq!(ownership[index]["todoTaskId"], *task_id);
        assert_eq!(ownership[index]["generation"], 7);
        assert_eq!(ownership[index].as_object().unwrap().len(), 3);
    }

    // Only the composite identity is on the wire: no task content, no paths.
    let encoded = serde_json::to_string(&projected).unwrap();
    assert!(!encoded.contains("task-content"));
    assert!(!wire_json_leaks_absolute_path(&encoded));

    // Deserialization tolerates absent ownership (wire default) and round-trips.
    let without = serde_json::from_value::<WorkflowWireSnapshot>(json!({
        "workflowId": "wf-own-1",
        "name": "alpha",
        "objective": "obj",
        "status": "running",
        "generation": 7,
    }))
    .expect("absent ownership defaults to empty");
    assert!(without.ownership.is_empty());
    let round = serde_json::from_value::<WorkflowWireSnapshot>(value).expect("round-trip");
    assert_eq!(round.ownership, projected.ownership);
}

#[test]
fn empty_todo_skips_ownership_on_the_wire() {
    let host_snap = WorkflowSnapshot {
        workflow_id: WorkflowId::new("wf-own-empty"),
        name: "alpha".into(),
        objective: "obj".into(),
        status: WorkflowStatus::Queued,
        created_at_ms: 1,
        updated_at_ms: 1,
        generation: 1,
        todo: pi_coding::TodoState {
            phases: Vec::new(),
            storage: pi_coding::TodoStorage::Memory,
        },
        worktree_label: None,
        branch: None,
        supervisor_agent_id: None,
        supervisor_job_id: None,
        failure: None,
        integration: pi_coding::WorkflowIntegration::None,
    };
    let value = serde_json::to_value(project_workflow_snapshot(&host_snap)).unwrap();
    assert!(
        value.get("ownership").is_none(),
        "empty ownership must be skipped: {value}"
    );
}

#[tokio::test]
async fn stdout_framing_is_one_json_object_per_lf() {
    let host = MemoryWorkflowRpcHost::new();
    let data = dispatch_workflow_command(
        &host,
        WorkflowRpcCommand::WorkflowCreate {
            id: Some("frame".into()),
            name: "frame".into(),
            objective: "check lf".into(),
        },
    )
    .await
    .unwrap();
    let response = json!({
        "id": "frame",
        "type": "response",
        "command": "workflow_create",
        "success": true,
        "data": data,
    });
    let event = project_workflow_event(&WorkflowEvent::Updated {
        snapshot: host.list().unwrap()[0].clone(),
    });

    let mut buffer = Vec::new();
    write_json_line(&mut buffer, &response).unwrap();
    write_json_line(&mut buffer, &event).unwrap();
    let text = String::from_utf8(buffer).unwrap();
    assert!(text.ends_with('\n'));
    assert!(
        !text.contains('\u{1b}'),
        "ANSI must not appear on structured stdout"
    );
    let lines: Vec<_> = text.lines().collect();
    assert_eq!(lines.len(), 2);
    for line in lines {
        let value: Value = serde_json::from_str(line).expect("one JSON object per LF");
        assert!(value.is_object());
        assert!(!wire_json_leaks_absolute_path(line));
    }
}

#[tokio::test]
async fn workflow_rpc_state_dispatches_and_reports_missing_host() {
    let state = WorkflowRpcState::new();
    let err = state
        .dispatch(WorkflowRpcCommand::WorkflowList { id: None })
        .await
        .expect_err("no host");
    assert!(err.to_string().contains("not available"), "{err:#}");

    state.set_memory_host();
    let listed = state
        .dispatch(WorkflowRpcCommand::WorkflowList { id: None })
        .await
        .expect("list");
    assert_eq!(listed["workflows"], json!([]));
}

#[test]
fn status_wire_values_match_canonical_contract() {
    for (status, expected) in [
        (WorkflowStatus::Queued, "queued"),
        (WorkflowStatus::Planning, "planning"),
        (WorkflowStatus::Running, "running"),
        (WorkflowStatus::Paused, "paused"),
        (WorkflowStatus::Integrating, "integrating"),
        (WorkflowStatus::Completed, "completed"),
        (WorkflowStatus::Failed, "failed"),
        (WorkflowStatus::Cancelled, "cancelled"),
        (WorkflowStatus::Conflicted, "conflicted"),
    ] {
        assert_eq!(serde_json::to_value(status).unwrap(), json!(expected));
        assert_eq!(status.as_str(), expected);
    }
}
