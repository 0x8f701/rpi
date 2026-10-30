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
        json!({"type":"workflow_detail","workflowId":"wf-1"}),
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
        durable_policy: crate::test_support::test_durable_policy_state(),
        worktree_label: Some(absolute.to_string_lossy().into_owned()),
        branch: Some("workflow/wf-event-1".into()),
        supervisor_agent_id: Some("sup-1".into()),
        supervisor_job_id: None,
        failure: None,
        integration: pi_coding::WorkflowIntegration::None,
        planning_prompts_consumed: 0,


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
                            waiting_on: Vec::new(),
                            agent: None,
                        },
                        pi_coding::TodoItem {
                            id: "task-b".into(),
                            content: "/private/task-content-b".into(),
                            status: pi_coding::TodoStatus::InProgress,
                            depends_on: Vec::new(),
                            ready: true,
                            waiting_on: Vec::new(),
                            agent: None,
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
                        waiting_on: Vec::new(),
                        agent: None,
                    }],
                },
            ],
            storage: pi_coding::TodoStorage::Memory,
        },
        durable_policy: crate::test_support::test_durable_policy_state(),
        worktree_label: Some("wf-own-1".into()),
        branch: None,
        supervisor_agent_id: None,
        supervisor_job_id: None,
        failure: None,
        integration: pi_coding::WorkflowIntegration::None,
        planning_prompts_consumed: 0,

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
        durable_policy: crate::test_support::test_durable_policy_state(),
        worktree_label: None,
        branch: None,
        supervisor_agent_id: None,
        supervisor_job_id: None,
        failure: None,
        integration: pi_coding::WorkflowIntegration::None,
        planning_prompts_consumed: 0,

    };
    let value = serde_json::to_value(project_workflow_snapshot(&host_snap)).unwrap();
    assert!(
        value.get("ownership").is_none(),
        "empty ownership must be skipped: {value}"
    );
}

#[tokio::test]
async fn workflow_detail_returns_panel_snapshot_with_redacted_worktree() {
    let host = MemoryWorkflowRpcHost::new();
    let created = dispatch_workflow_command(
        &host,
        WorkflowRpcCommand::WorkflowCreate {
            id: Some("d1".into()),
            name: "ship".into(),
            objective: "land detail".into(),
        },
    )
    .await
    .expect("create");
    let workflow_id = created["workflowId"].as_str().unwrap().to_owned();

    let detail = dispatch_workflow_command(
        &host,
        WorkflowRpcCommand::WorkflowDetail {
            id: Some("detail-1".into()),
            workflow_id: Some(workflow_id.clone()),
            name: None,
        },
    )
    .await
    .expect("detail");
    // Panel projection: identity, objective, and the queued status.
    assert_eq!(detail["id"], workflow_id);
    assert_eq!(detail["name"], "ship");
    assert_eq!(detail["objective"], "land detail");
    assert_eq!(detail["status"], "queued");
    assert_eq!(detail["generation"], 1);
    // Empty live sections project as defaults, not errors. The durable
    // fallback carries no live jobs/irc, and the supervisor actor has no
    // authoritative role (role omitted), so the Web reads them as unavailable
    // live-only data rather than fabricated entries.
    assert_eq!(detail["activeTasks"], json!([]));
    assert_eq!(detail["subagents"], json!([]));
    assert_eq!(detail["jobs"], json!([]), "fallback jobs is an empty array");
    assert_eq!(
        detail["recentIrc"],
        json!([]),
        "fallback irc is an empty array"
    );
    assert_eq!(
        detail["supervisor"]["role"],
        json!(null),
        "fallback supervisor has no authoritative role"
    );
    assert!(
        detail["supervisor"].get("role").is_none() || detail["supervisor"]["role"].is_null(),
        "role omitted or null on durable fallback"
    );
    // The memory host stores an absolute worktree path; the wire label must
    // be redacted to a display-safe basename.
    let worktree = detail["worktree"]["label"]
        .as_str()
        .expect("worktree label");
    assert!(
        !Path::new(worktree).is_absolute(),
        "worktree must not be absolute: {worktree}"
    );
    let encoded = serde_json::to_string(&detail).unwrap();
    assert!(
        !wire_json_leaks_absolute_path(&encoded),
        "absolute path leaked: {encoded}"
    );

    // By-name lookup works the same way.
    let by_name = dispatch_workflow_command(
        &host,
        WorkflowRpcCommand::WorkflowDetail {
            id: None,
            workflow_id: None,
            name: Some("ship".into()),
        },
    )
    .await
    .expect("detail by name");
    assert_eq!(by_name["id"], workflow_id);

    // A missing selector fails closed.
    let err = dispatch_workflow_command(
        &host,
        WorkflowRpcCommand::WorkflowDetail {
            id: None,
            workflow_id: None,
            name: None,
        },
    )
    .await
    .expect_err("selector required");
    assert!(err.to_string().contains("workflowId or name"), "{err}");
    // An unknown workflow fails too.
    let err = dispatch_workflow_command(
        &host,
        WorkflowRpcCommand::WorkflowDetail {
            id: None,
            workflow_id: Some("missing".into()),
            name: None,
        },
    )
    .await
    .expect_err("unknown workflow");
    assert!(err.to_string().contains("missing"), "{err}");
}

#[tokio::test]
async fn workflow_detail_round_trips_through_rpc_state() {
    let state = WorkflowRpcState::new();
    state.set_memory_host();
    state
        .dispatch(WorkflowRpcCommand::WorkflowCreate {
            id: Some("state-d".into()),
            name: "detail-state".into(),
            objective: "check state dispatch".into(),
        })
        .await
        .expect("create");
    let listed = state
        .dispatch(WorkflowRpcCommand::WorkflowList { id: None })
        .await
        .expect("list");
    let workflow_id = listed["workflows"][0]["workflowId"]
        .as_str()
        .unwrap()
        .to_owned();
    let detail = state
        .dispatch(WorkflowRpcCommand::WorkflowDetail {
            id: Some("state-detail".into()),
            workflow_id: Some(workflow_id.clone()),
            name: None,
        })
        .await
        .expect("detail");
    assert_eq!(detail["id"], workflow_id);
    assert_eq!(detail["name"], "detail-state");
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

#[test]
fn workflow_detail_wire_exposes_authoritative_jobs_irc_and_actor_roles() {
    // The RPC workflow-detail payload (a WorkflowPanelSnapshot projected from
    // the core WorkflowRuntimeDetail) must expose the full authoritative job
    // shape, actor role/definition separate from the display name, and IRC
    // correlation fields — all in stable camelCase, with no path leakage.
    let snapshot = WorkflowSnapshot {
        workflow_id: WorkflowId::new("wf-detail-wire"),
        name: "Detail wire".into(),
        objective: "project the detail".into(),
        status: WorkflowStatus::Running,
        created_at_ms: 1,
        updated_at_ms: 2,
        generation: 7,
        todo: pi_coding::TodoState {
            phases: Vec::new(),
            storage: pi_coding::TodoStorage::Memory,
        },
        durable_policy: crate::test_support::test_durable_policy_state(),
        worktree_label: Some("wf-detail-wire".into()),
        branch: Some("rpi/workflow/detail-wire".into()),
        supervisor_agent_id: None,
        supervisor_job_id: None,
        failure: None,
        integration: pi_coding::WorkflowIntegration::None,
        planning_prompts_consumed: 0,

    };
    let detail = pi_coding::WorkflowRuntimeDetail {
        workflow_id: snapshot.workflow_id.clone(),
        generation: 7,
        name: "Detail wire".into(),
        objective: "project the detail".into(),
        status: WorkflowStatus::Running,
        failure: None,
        todo: snapshot.todo.clone(),
        supervisor: Some(pi_coding::WorkflowSupervisorDetail {
            display_name: "Supervisor".into(),
            role: "supervisor".into(),
            status: pi_coding::AgentStatus::Running,
            task_summary: Some("Direct workers".into()),
            todo_task_id: None,
            activity: Vec::new(),
        }),
        subagents: vec![pi_coding::WorkflowSubagentDetail {
            display_name: "Code Review".into(),
            role: "reviewer".into(),
            status: pi_coding::AgentStatus::Idle,
            task_summary: Some("Review changes".into()),
            todo_task_id: Some("render-id".into()),
            activity: Vec::new(),
        }],
        jobs: vec![pi_coding::WorkflowRuntimeJobDetail {
            display_name: "Code Review".into(),
            job_id: "job-1".into(),
            role: "reviewer".into(),
            status: pi_coding::JobStatus::Running,
            task_summary: Some("Run integration".into()),
            todo_task_id: Some("render-id".into()),
            created_at_ms: 3,
            started_at_ms: Some(4),
            finished_at_ms: None,
            soft_budget_exhausted: true,
        }],
        irc: vec![pi_coding::WorkflowIrcMessage {
            message_id: "msg-1".into(),
            from: "Code Review".into(),
            to: "Supervisor".into(),
            body: "Live update".into(),
            timestamp_ms: 5,
            reply_to: Some("msg-0".into()),
        }],
        supervisor_activity: Vec::new(),
        planning_started_at_ms: None,
    };
    let panel =
        crate::workflow_panel::WorkflowPanelSnapshot::from_runtime_detail(&detail, &snapshot);
    let data = serde_json::to_value(&panel).expect("serialize panel");

    // workflowId + generation are preserved strictly.
    assert_eq!(data["id"], "wf-detail-wire");
    assert_eq!(data["generation"], 7);

    // Jobs array: full authoritative shape in camelCase.
    let job = &data["jobs"][0];
    assert_eq!(job["jobId"], "job-1", "jobId exposed: {data}");
    assert_eq!(job["displayName"], "Code Review");
    assert_eq!(
        job["role"], "reviewer",
        "job role separate from display name"
    );
    assert_eq!(job["status"], "running");
    assert_eq!(job["taskSummary"], "Run integration");
    assert_eq!(job["todoTaskId"], "render-id");
    assert_eq!(job["createdAtMs"], 3);
    assert_eq!(job["startedAtMs"], 4);
    assert!(
        job.get("finishedAtMs").is_none(),
        "absent finishedAtMs must be omitted"
    );
    assert_eq!(
        job["softBudgetExhausted"], true,
        "softBudgetExhausted emitted when true"
    );
    // No model field is invented (no authoritative runtime source for it).
    assert!(job.get("model").is_none(), "no model field is fabricated");

    // Actors: display name (`name`) and role are separate keys.
    assert_eq!(data["supervisor"]["name"], "Supervisor");
    assert_eq!(
        data["supervisor"]["role"], "supervisor",
        "supervisor role separate from name"
    );
    assert_eq!(data["subagents"][0]["name"], "Code Review");
    assert_eq!(
        data["subagents"][0]["role"], "reviewer",
        "subagent role separate from name"
    );

    // IRC: from (sender), to, body (text), timestamp, messageId, replyTo.
    let irc = &data["recentIrc"][0];
    assert_eq!(irc["sender"], "Code Review");
    assert_eq!(irc["to"], "Supervisor");
    assert_eq!(irc["text"], "Live update");
    assert_eq!(irc["atMs"], 5);
    assert_eq!(irc["messageId"], "msg-1", "messageId exposed for threading");
    assert_eq!(irc["replyTo"], "msg-0", "replyTo exposed when present");

    // The opaque correlation ids are on the wire by design; no path leaks.
    let encoded = serde_json::to_string(&panel).expect("encode");
    assert!(
        !wire_json_leaks_absolute_path(&encoded),
        "absolute path leaked: {encoded}"
    );
    assert!(encoded.contains("\"jobId\":\"job-1\""));
    assert!(encoded.contains("\"messageId\":\"msg-1\""));
    assert!(encoded.contains("\"replyTo\":\"msg-0\""));
    assert!(encoded.contains("\"softBudgetExhausted\":true"));
}
