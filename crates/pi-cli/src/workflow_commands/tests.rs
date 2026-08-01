use super::*;
use pi_coding::{
    TodoState, TodoStorage, WorkflowFailure, WorkflowRuntimeFactory, WorkflowRuntimeIdentity,
    WorkflowRuntimeRequest, WorkflowRuntimeUpdate,
};
use std::sync::Arc;

#[test]
fn primary_parse_contract_covers_all_subcommands() {
    assert_eq!(
        parse_interactive_workflow_command(None).unwrap(),
        InteractiveWorkflowCommand::OpenPage
    );
    assert_eq!(
        parse_interactive_workflow_command(Some("")).unwrap(),
        InteractiveWorkflowCommand::OpenPage
    );
    assert_eq!(
        parse_interactive_workflow_command(Some("   ")).unwrap(),
        InteractiveWorkflowCommand::OpenPage
    );
    assert_eq!(
        parse_interactive_workflow_command(Some("list")).unwrap(),
        InteractiveWorkflowCommand::List
    );
    assert_eq!(
        parse_interactive_workflow_command(Some("show")).unwrap(),
        InteractiveWorkflowCommand::Show { selector: None }
    );
    assert_eq!(
        parse_interactive_workflow_command(Some("show wf-1")).unwrap(),
        InteractiveWorkflowCommand::Show {
            selector: Some(WorkflowSelector("wf-1".into())),
        }
    );
    assert_eq!(
        parse_interactive_workflow_command(Some("show release")).unwrap(),
        InteractiveWorkflowCommand::Show {
            selector: Some(WorkflowSelector("release".into())),
        }
    );
    assert_eq!(
        parse_interactive_workflow_command(Some(
            r#"create "ship it" "land the multi workflow foundation""#
        ))
        .unwrap(),
        InteractiveWorkflowCommand::Create {
            name: "ship it".into(),
            objective: "land the multi workflow foundation".into(),
        }
    );
    assert_eq!(
        parse_interactive_workflow_command(Some("create release land the feature")).unwrap(),
        InteractiveWorkflowCommand::Create {
            name: "release".into(),
            objective: "land the feature".into(),
        }
    );
    assert_eq!(
        parse_interactive_workflow_command(Some("pause")).unwrap(),
        InteractiveWorkflowCommand::Pause { selector: None }
    );
    assert_eq!(
        parse_interactive_workflow_command(Some("pause wf-9")).unwrap(),
        InteractiveWorkflowCommand::Pause {
            selector: Some(WorkflowSelector("wf-9".into())),
        }
    );
    assert_eq!(
        parse_interactive_workflow_command(Some("resume release")).unwrap(),
        InteractiveWorkflowCommand::Resume {
            selector: Some(WorkflowSelector("release".into())),
        }
    );
    assert_eq!(
        parse_interactive_workflow_command(Some("cancel")).unwrap(),
        InteractiveWorkflowCommand::Cancel { selector: None }
    );
    assert_eq!(
        parse_interactive_workflow_command(Some("integrate wf-2")).unwrap(),
        InteractiveWorkflowCommand::Integrate {
            selector: Some(WorkflowSelector("wf-2".into())),
        }
    );
    assert_eq!(
        parse_interactive_workflow_command(Some("remove old")).unwrap(),
        InteractiveWorkflowCommand::Remove {
            selector: Some(WorkflowSelector("old".into())),
        }
    );
}

#[test]
fn rejects_unknown_and_malformed_subcommands_with_usage() {
    let err = parse_interactive_workflow_command(Some("workfloww")).unwrap_err();
    assert!(
        err.to_string().contains("unknown workflow subcommand"),
        "{err:#}"
    );
    assert!(err.to_string().contains(WORKFLOW_USAGE), "{err:#}");

    let err = parse_interactive_workflow_command(Some("list extra")).unwrap_err();
    assert!(err.to_string().contains("unexpected argument"), "{err:#}");

    let err = parse_interactive_workflow_command(Some("show a b")).unwrap_err();
    assert!(err.to_string().contains("unexpected argument"), "{err:#}");

    let err = parse_interactive_workflow_command(Some("create only-name")).unwrap_err();
    assert!(
        err.to_string().contains("create requires <name> <objective>"),
        "{err:#}"
    );

    let err = parse_interactive_workflow_command(Some("create")).unwrap_err();
    assert!(
        err.to_string().contains("create requires <name> <objective>"),
        "{err:#}"
    );

    let err = parse_interactive_workflow_command(Some("create \"\" objective")).unwrap_err();
    assert!(
        err.to_string().contains("name must not be empty")
            || err.to_string().contains("create requires"),
        "{err:#}"
    );
}

#[test]
fn quoted_args_preserve_spaces_via_shared_parser() {
    let command = parse_interactive_workflow_command(Some(
        "create 'alpha beta' \"objective with spaces\"",
    ))
    .unwrap();
    assert_eq!(
        command,
        InteractiveWorkflowCommand::Create {
            name: "alpha beta".into(),
            objective: "objective with spaces".into(),
        }
    );
}

#[test]
fn status_wire_strings_are_exact_and_complete() {
    let expected = [
        "queued",
        "planning",
        "running",
        "paused",
        "integrating",
        "completed",
        "failed",
        "cancelled",
        "conflicted",
    ];
    for value in expected {
        let status = parse_workflow_status(value).unwrap();
        assert_eq!(status.as_str(), value);
    }
    assert!(parse_workflow_status("done").is_err());
    assert!(WorkflowStatus::Running.is_active());
    assert!(!WorkflowStatus::Completed.is_active());
    assert!(WorkflowStatus::Completed.is_terminal());
    assert!(!WorkflowStatus::Failed.is_active());
    assert!(!WorkflowStatus::Cancelled.is_active());
    assert!(!WorkflowStatus::Conflicted.is_active());
}

#[test]
fn human_formatting_is_english_only() {
    let items = vec![
        summary("wf-1", "release", "ship it", WorkflowStatus::Running),
        summary("wf-2", "docs", "write docs", WorkflowStatus::Completed),
    ];
    assert_eq!(
        format_workflows_header(1, 2),
        "Workflows · 1 active · 2 total"
    );
    assert_eq!(count_active_workflows(&items), 1);
    let list = format_workflow_list(&items);
    assert!(list.starts_with("Workflows · 1 active · 2 total\n"));
    assert!(list.contains("running · release · wf-1 · ship it"));
    assert!(list.contains("completed · docs · wf-2 · write docs"));
    assert_eq!(format_workflow_list(&[]), "No workflows.");

    let detail = format_workflow_detail(&WorkflowSummary {
        worktree_path: Some("workspaces/wf-1".into()),
        branch: Some("rpi/workflow/release".into()),
        supervisor_agent_id: Some("sup-1".into()),
        failure: Some("merge conflict".into()),
        integration: Some("conflicted · a.rs".into()),
        ..summary("wf-1", "release", "ship it", WorkflowStatus::Conflicted)
    });
    assert!(detail.contains("Name: release"));
    assert!(detail.contains("Status: conflicted"));
    assert!(detail.contains("Worktree: wf-1"));
    assert!(detail.contains("Branch: rpi/workflow/release"));
    assert!(detail.contains("Supervisor: sup-1"));
    assert!(detail.contains("Failure: merge conflict"));
    assert!(detail.contains("Integration: conflicted · a.rs"));
    for label in [
        "Name:",
        "Id:",
        "Status:",
        "Objective:",
        "Worktree:",
        "Branch:",
        "Supervisor:",
        "Failure:",
        "Integration:",
    ] {
        assert!(detail.contains(label), "missing English label {label}");
    }
}

#[test]
fn snapshot_projection_is_lossless_for_cli_fields() {
    let snapshot = WorkflowSnapshot {
        workflow_id: WorkflowId::new("wf-9"),
        name: "release".into(),
        objective: "ship".into(),
        status: WorkflowStatus::Running,
        created_at_ms: 1,
        updated_at_ms: 2,
        generation: 4,
        todo: TodoState {
            phases: Vec::new(),
            storage: TodoStorage::Session,
        },
        worktree_label: Some("workspaces/wf-9".into()),
        branch: Some("rpi/workflow/wf-9".into()),
        supervisor_agent_id: Some("sup".into()),
        supervisor_job_id: Some("job".into()),
        failure: Some(WorkflowFailure {
            message: "boom".into(),
        }),
        integration: WorkflowIntegration::Conflicted {
            conflicts: vec!["a.rs".into()],
        },
    };
    let summary = WorkflowSummary::from(&snapshot);
    assert_eq!(summary.id.as_str(), "wf-9");
    assert_eq!(summary.name, "release");
    assert_eq!(summary.objective, "ship");
    assert_eq!(summary.status, WorkflowStatus::Running);
    assert_eq!(summary.generation, 4);
    assert_eq!(summary.worktree_path.as_deref(), Some("workspaces/wf-9"));
    assert_eq!(summary.branch.as_deref(), Some("rpi/workflow/wf-9"));
    assert_eq!(summary.supervisor_agent_id.as_deref(), Some("sup"));
    assert_eq!(summary.failure.as_deref(), Some("boom"));
    assert_eq!(summary.integration.as_deref(), Some("conflicted · a.rs"));
}

#[tokio::test]
async fn adapter_executes_through_real_manager_lifecycle() {
    let root = tempfile::tempdir().expect("tempdir");
    let port = ManagerWorkflowPort::new(
        WorkflowManager::open_with_factory(root.path(), Arc::new(TestRuntimeFactory))
            .expect("manager"),
    );

    let effect = execute_interactive_workflow_command(
        &port,
        parse_interactive_workflow_command(None).unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(effect, WorkflowCommandEffect::OpenPage);

    let effect = execute_interactive_workflow_command(
        &port,
        parse_interactive_workflow_command(Some(
            r#"create "ship it" "land multi workflow""#,
        ))
        .unwrap(),
    )
    .await
    .unwrap();
    let WorkflowCommandEffect::Message(created) = effect else {
        panic!("expected message");
    };
    assert!(created.contains("Name: ship it"));
    assert!(created.contains("Status: queued"));
    assert!(created.contains("Objective: land multi workflow"));
    assert!(created.contains("Branch: rpi/workflow/"));

    let effect = execute_interactive_workflow_command(
        &port,
        parse_interactive_workflow_command(Some("list")).unwrap(),
    )
    .await
    .unwrap();
    let WorkflowCommandEffect::Message(list) = effect else {
        panic!("expected message");
    };
    assert!(list.contains("Workflows · 1 active · 1 total"));
    assert!(list.contains("ship it"));

    let id = port.list().await.unwrap()[0].id.as_str().to_owned();
    let effect = execute_interactive_workflow_command(
        &port,
        parse_interactive_workflow_command(Some(&format!("show {id}"))).unwrap(),
    )
    .await
    .unwrap();
    assert!(matches!(effect, WorkflowCommandEffect::Message(message) if message.contains(&id)));

    let effect = execute_interactive_workflow_command(
        &port,
        parse_interactive_workflow_command(Some(&format!("pause {id}"))).unwrap(),
    )
    .await
    .unwrap();
    assert!(matches!(effect, WorkflowCommandEffect::Message(message) if message.contains("paused")));

    let effect = execute_interactive_workflow_command(
        &port,
        parse_interactive_workflow_command(Some(&format!("resume {id}"))).unwrap(),
    )
    .await
    .unwrap();
    assert!(matches!(effect, WorkflowCommandEffect::Message(message) if message.contains("running")));

    let effect = execute_interactive_workflow_command(
        &port,
        parse_interactive_workflow_command(Some(&format!("cancel {id}"))).unwrap(),
    )
    .await
    .unwrap();
    assert!(matches!(effect, WorkflowCommandEffect::Message(message) if message.contains("cancelled")));

    let effect = execute_interactive_workflow_command(
        &port,
        parse_interactive_workflow_command(Some(&format!("remove {id}"))).unwrap(),
    )
    .await
    .unwrap();
    let WorkflowCommandEffect::Message(message) = effect else {
        panic!("expected message");
    };
    assert!(message.starts_with("removed "));
    assert!(port.list().await.unwrap().is_empty());
}

fn summary(id: &str, name: &str, objective: &str, status: WorkflowStatus) -> WorkflowSummary {
    WorkflowSummary {
        id: WorkflowId::new(id),
        name: name.into(),
        objective: objective.into(),
        status,
        worktree_path: None,
        branch: None,
        supervisor_agent_id: None,
        generation: 1,
        failure: None,
        integration: None,
    }
}

struct TestRuntimeFactory;

#[async_trait::async_trait]
impl WorkflowRuntimeFactory for TestRuntimeFactory {
    async fn create(&self, request: &WorkflowRuntimeRequest) -> Result<WorkflowRuntimeIdentity> {
        Ok(WorkflowRuntimeIdentity {
            worktree_label: Some(format!("workspaces/{}", request.workflow_id.as_str())),
            branch: Some(format!("rpi/workflow/{}", request.workflow_id.as_str())),
            supervisor_agent_id: Some(format!("sup-{}", request.workflow_id.as_str())),
            supervisor_job_id: None,
            todo: TodoState {
                phases: Vec::new(),
                storage: TodoStorage::Session,
            },
        })
    }

    async fn restore(
        &self,
        request: &WorkflowRuntimeRequest,
        _snapshot: &WorkflowSnapshot,
    ) -> Result<WorkflowRuntimeIdentity> {
        self.create(request).await
    }

    async fn pause(&self, snapshot: &WorkflowSnapshot) -> Result<WorkflowRuntimeUpdate> {
        Ok(update(snapshot, WorkflowStatus::Paused))
    }

    async fn resume(&self, snapshot: &WorkflowSnapshot) -> Result<WorkflowRuntimeUpdate> {
        Ok(update(snapshot, WorkflowStatus::Running))
    }

    async fn cancel(&self, snapshot: &WorkflowSnapshot) -> Result<WorkflowRuntimeUpdate> {
        Ok(update(snapshot, WorkflowStatus::Cancelled))
    }

    async fn integrate(&self, snapshot: &WorkflowSnapshot) -> Result<WorkflowRuntimeUpdate> {
        Ok(WorkflowRuntimeUpdate {
            status: WorkflowStatus::Completed,
            todo: snapshot.todo.clone(),
            supervisor_agent_id: snapshot.supervisor_agent_id.clone(),
            supervisor_job_id: snapshot.supervisor_job_id.clone(),
            failure: None,
            integration: WorkflowIntegration::Applied {
                result_commit: "abc123".into(),
            },
        })
    }

    async fn remove(&self, _snapshot: &WorkflowSnapshot) -> Result<()> {
        Ok(())
    }
}

fn update(snapshot: &WorkflowSnapshot, status: WorkflowStatus) -> WorkflowRuntimeUpdate {
    WorkflowRuntimeUpdate {
        status,
        todo: snapshot.todo.clone(),
        supervisor_agent_id: snapshot.supervisor_agent_id.clone(),
        supervisor_job_id: snapshot.supervisor_job_id.clone(),
        failure: None,
        integration: snapshot.integration.clone(),
    }
}
