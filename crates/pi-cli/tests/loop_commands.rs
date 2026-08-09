use pi_agent::ThinkingLevel;
use pi_ai::Model;
use pi_ai::providers::{FauxProviderOptions, register_faux_provider};
use pi_coding::{Application, Session, SessionOptions};
use pi_cli::loop_commands::{execute_interactive_loop_command, parse_interactive_loop_command};

#[tokio::test]
async fn cli_loop_commands_drive_application_create_list_update_delete_and_cancel() {
    let mut model = Model::default();
    model.id = "faux-loop-cli".into();
    model.name = "Faux Loop CLI".into();
    model.api = "faux-loop-cli-test".into();
    model.provider = "faux-loop-cli-provider".into();
    model.base_url = "http://localhost:0".into();
    let registration = register_faux_provider(FauxProviderOptions {
        api: model.api.clone(),
        provider: model.provider.clone(),
        models: vec![model.clone()],
        chunk_size: 4,
    });
    let cwd = tempfile::tempdir().expect("cwd");
    let session = Session::new(SessionOptions {
        model,
        cwd: cwd.path().to_path_buf(),
        system_prompt: String::new(),
        thinking_level: ThinkingLevel::Off,
        api_key: String::new(),
        compaction: None,
        stream_options: Default::default(),
        tools: Some(Vec::new()),
        before_tool_call: None,
        after_tool_call: None,
        stream_fn: None,
        auth_resolver: None,
    })
    .expect("session");
    let application = Application::new(session).await;

    let seconds_create = parse_interactive_loop_command("loop", Some("3s echo hello"))
        .expect("parse exact seconds")
        .expect("seconds create command");
    let seconds_output = execute_interactive_loop_command(&application, seconds_create)
        .await
        .expect("execute exact seconds");
    assert!(seconds_output.contains("every 3 seconds"));
    let seconds_id = seconds_output
        .strip_prefix("scheduled ")
        .and_then(|rest| rest.split_once(" · ").map(|(id, _)| id))
        .expect("seconds task id")
        .to_owned();
    let seconds_task = application
        .loop_list()
        .await
        .expect("list seconds task")
        .into_iter()
        .find(|task| task.id == seconds_id)
        .expect("created seconds task");
    assert_eq!(seconds_task.interval_secs, 3);
    assert_eq!(seconds_task.human_schedule(), "every 3 seconds");
    assert_eq!(
        (seconds_task.next_fire_at() - seconds_task.created_at).num_seconds(),
        3
    );
    application.loop_cancel(&seconds_id).await.expect("cancel seconds task");

    let bare_create = parse_interactive_loop_command("loop", Some("300 bare seconds"))
        .expect("parse bare seconds")
        .expect("bare seconds create command");
    let bare_output = execute_interactive_loop_command(&application, bare_create)
        .await
        .expect("execute bare seconds");
    assert!(bare_output.contains("every 5 minutes"));
    let bare_id = bare_output
        .strip_prefix("scheduled ")
        .and_then(|rest| rest.split_once(" · ").map(|(id, _)| id))
        .expect("bare seconds task id")
        .to_owned();
    application.loop_cancel(&bare_id).await.expect("cancel bare seconds task");

    let create = parse_interactive_loop_command("loop", Some("1h cli scheduled"))
        .expect("parse create")
        .expect("create command");
    let created = execute_interactive_loop_command(&application, create)
        .await
        .expect("execute create");
    let task_id = created
        .strip_prefix("scheduled ")
        .and_then(|rest| rest.split_once(" · ").map(|(id, _)| id))
        .expect("created output contains task id")
        .to_owned();

    let list = parse_interactive_loop_command("loop", Some("list"))
        .expect("parse list")
        .expect("list command");
    let listed = execute_interactive_loop_command(&application, list)
        .await
        .expect("execute list");
    assert!(listed.contains(&task_id));
    assert!(listed.contains("cli scheduled"));

    let update = parse_interactive_loop_command(
        "loop",
        Some(&format!("update {task_id} 2h cli updated")),
    )
    .expect("parse update")
    .expect("update command");
    let updated = execute_interactive_loop_command(&application, update)
        .await
        .expect("execute update");
    assert!(updated.contains("every 2 hours"));
    assert!(updated.contains("cli updated"));

    let delete = parse_interactive_loop_command("loop", Some(&format!("delete {task_id}")))
        .expect("parse delete")
        .expect("delete command");
    assert_eq!(
        execute_interactive_loop_command(&application, delete)
            .await
            .expect("execute delete"),
        format!("deleted loop {task_id}")
    );

    let second_create = parse_interactive_loop_command("loop", Some("1h cancel me"))
        .expect("parse second create")
        .expect("second create command");
    let second_created = execute_interactive_loop_command(&application, second_create)
        .await
        .expect("execute second create");
    let second_id = second_created
        .strip_prefix("scheduled ")
        .and_then(|rest| rest.split_once(" · ").map(|(id, _)| id))
        .expect("second task id")
        .to_owned();
    let cancel = parse_interactive_loop_command("loop", Some(&format!("cancel {second_id}")))
        .expect("parse cancel")
        .expect("cancel command");
    assert_eq!(
        execute_interactive_loop_command(&application, cancel)
            .await
            .expect("execute cancel"),
        format!("cancelled loop {second_id}")
    );

    // The explicit `create` subcommand is the same operation as the bare form.
    let explicit_create = parse_interactive_loop_command(
        "loop",
        Some("create 1h explicit create subcommand"),
    )
    .expect("parse explicit create")
    .expect("explicit create command");
    let explicit_created = execute_interactive_loop_command(&application, explicit_create)
        .await
        .expect("execute explicit create");
    assert!(explicit_created.contains("every 1 hour"));
    let explicit_id = explicit_created
        .strip_prefix("scheduled ")
        .and_then(|rest| rest.split_once(" · ").map(|(id, _)| id))
        .expect("explicit task id")
        .to_owned();
    application
        .loop_cancel(&explicit_id)
        .await
        .expect("cancel explicit task");

    // Legacy alias `/loops` still lists (muscle-memory compatibility).
    let empty = execute_interactive_loop_command(
        &application,
        parse_interactive_loop_command("loops", None)
            .expect("parse final list")
            .expect("final list command"),
    )
    .await
    .expect("execute final list");
    assert_eq!(empty, "no active loops");

    application.cleanup().await;
    registration.unregister();
}
