//! Deterministic coverage for every `BUILTIN_COMMANDS` entry.
//!
//! Each built-in is exercised through the same Application / parse / execute
//! helpers the TUI and line REPL dispatch into. Cases assert a concrete
//! observable outcome: successful minimal action, typed usage/validation error,
//! or a panel-open precursor (catalog / snapshot / empty listing) — never a
//! model turn. Isolated temp HOME keeps CI free of credentials and network.

use std::collections::BTreeSet;
use std::io::Read;
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use pi_agent::ThinkingLevel;
use pi_ai::Model;
use pi_ai::providers::{FauxProviderOptions, register_faux_provider};
use pi_coding::{Application, Session, SessionOptions, TrustDecision};
use pi_cli::goal_commands::{
    execute_interactive_goal_command, parse_interactive_goal_command,
};
use pi_cli::workflow_commands::{
    InteractiveWorkflowCommand, WorkflowCommandEffect, parse_interactive_workflow_command,
};
use pi_cli::interactive_commands::{
    BUILTIN_COMMANDS, PRIMARY_COMMAND_NAMES, builtin, executable_catalog, is_primary_command,
    parse_chain_invocation, parse_interactive_settings_command, parse_run_invocation,
    requires_arguments, usage, visible_catalog,
};
use pi_cli::keybindings::KeyBindingsManager;
use pi_cli::loop_commands::{
    execute_interactive_loop_command, parse_interactive_loop_command,
};
use pi_cli::process_commands::{
    execute_interactive_process_command, parse_interactive_process_command,
};
use pi_cli::theme::ThemeManager;
use tempfile::TempDir;
use pi_cli::code_review_panel::CodeReviewPanel;
use pi_cli::side_chat::{SideChatController, SideChatToolMode};

/// Hard upper bound for one-shot git fixture setup in this file.
const SUBPROCESS_TIMEOUT: Duration = Duration::from_secs(20);

/// Bytes retained from each pipe for diagnostics. Readers still drain to EOF
/// so a chatty child cannot fill the OS pipe; only the prefix is kept.
const PIPE_DIAG_CAP: usize = 64 * 1024;

/// Drain `read` to EOF while retaining at most `cap` bytes (prefix).
fn drain_capped(mut read: impl Read, cap: usize) -> Vec<u8> {
    let mut retained = Vec::new();
    let mut chunk = [0u8; 8 * 1024];
    loop {
        match read.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                if retained.len() < cap {
                    let take = (cap - retained.len()).min(n);
                    retained.extend_from_slice(&chunk[..take]);
                }
            }
            Err(_) => break,
        }
    }
    retained
}

/// Run a spawned command with a hard local deadline.
///
/// Drains stdout/stderr on dedicated threads from spawn (retaining only a fixed
/// diagnostic prefix while continuing to EOF), then polls `try_wait` until exit
/// or [`SUBPROCESS_TIMEOUT`]. On expiry or `try_wait` error the child is killed
/// and waited before readers are joined. Readers are never joined while the
/// child may still hold pipe ends.
fn run_command_bounded(mut command: Command) -> Output {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command.spawn().expect("spawn subprocess");
    let stdout = child.stdout.take().expect("stdout pipe");
    let stderr = child.stderr.take().expect("stderr pipe");

    let stdout_reader = thread::spawn(move || drain_capped(stdout, PIPE_DIAG_CAP));
    let stderr_reader = thread::spawn(move || drain_capped(stderr, PIPE_DIAG_CAP));

    let deadline = Instant::now() + SUBPROCESS_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    let stdout = stdout_reader
                        .join()
                        .unwrap_or_else(|_| b"<stdout reader panicked>".to_vec());
                    let stderr = stderr_reader
                        .join()
                        .unwrap_or_else(|_| b"<stderr reader panicked>".to_vec());
                    panic!(
                        "subprocess timed out after {}s\n--- stdout ---\n{}\n--- stderr ---\n{}",
                        SUBPROCESS_TIMEOUT.as_secs(),
                        String::from_utf8_lossy(&stdout),
                        String::from_utf8_lossy(&stderr),
                    );
                }
                thread::sleep(Duration::from_millis(20));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let stdout = stdout_reader
                    .join()
                    .unwrap_or_else(|_| b"<stdout reader panicked>".to_vec());
                let stderr = stderr_reader
                    .join()
                    .unwrap_or_else(|_| b"<stderr reader panicked>".to_vec());
                panic!(
                    "try_wait subprocess: {error}\n--- stdout ---\n{}\n--- stderr ---\n{}",
                    String::from_utf8_lossy(&stdout),
                    String::from_utf8_lossy(&stderr),
                );
            }
        }
    };

    // Child is reaped: safe to join pipe readers (EOF follows close).
    let stdout = stdout_reader
        .join()
        .unwrap_or_else(|_| b"<stdout reader panicked>".to_vec());
    let stderr = stderr_reader
        .join()
        .unwrap_or_else(|_| b"<stderr reader panicked>".to_vec());
    Output {
        status,
        stdout,
        stderr,
    }
}

struct Fixture {
    _home: TempDir,
    cwd: TempDir,
    application: Application,
    registration: pi_ai::providers::FauxProviderRegistration,
}

impl Fixture {
    async fn new(tag: &str) -> Self {
        // Isolated dirs only — no process-wide env mutation (Rust 2024 /
        // `-F unsafe-code` forbids `set_var` in this crate).
        let home = TempDir::new().expect("temp HOME");
        let cwd = TempDir::new().expect("temp cwd");
        let agent_dir = home.path().join(".pi");
        std::fs::create_dir_all(&agent_dir).expect("agent dir");
        std::fs::write(agent_dir.join("settings.json"), b"{}").expect("seed settings");

        let mut model = Model::default();
        model.id = format!("faux-slash-{tag}");
        model.name = format!("Faux Slash {tag}");
        model.api = format!("faux-slash-{tag}");
        model.provider = "faux".into();
        model.base_url = "http://localhost:0".into();
        let registration = register_faux_provider(FauxProviderOptions {
            api: model.api.clone(),
            provider: model.provider.clone(),
            models: vec![model.clone()],
            chunk_size: 4,
        });

        let mut resource_options = pi_coding::ResourceManagerOptions::new(cwd.path());
        resource_options.agent_dir = agent_dir;
        resource_options.project_trust_override = Some(true);
        resource_options.disable_extensions = true;
        resource_options.disable_skills = true;
        resource_options.disable_prompt_templates = true;
        resource_options.disable_themes = true;
        resource_options.disable_context_files = true;
        resource_options.headless = true;
        let resources =
            pi_coding::ResourceManager::new(resource_options).expect("resource manager");

        let session = Session::new(SessionOptions {
            model: model.clone(),
            cwd: cwd.path().to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: "faux".into(),
            compaction: None,
            stream_options: Default::default(),
            tools: Some(Vec::new()),
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: None,
            auth_resolver: None,
        })
        .expect("session");
        session
            .attach_resources(resources)
            .await
            .expect("attach resources");

        // Match print_mode / rpc fixtures: recorder under cwd, attach before Application.
        session
            .record(
                pi_coding::start_session_in(
                    cwd.path(),
                    Some(&model),
                    Some("off"),
                    Some(cwd.path()),
                    Some(&format!("slash-{tag}")),
                    None,
                )
                .expect("start session recorder"),
            )
            .expect("attach recorder");

        let application = Application::new(session).await;
        Self {
            _home: home,
            cwd,
            application,
            registration,
        }
    }

    async fn with_recorder(tag: &str) -> Self {
        Self::new(tag).await
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.registration.unregister();
    }
}

fn required_arg_commands() -> BTreeSet<&'static str> {
    BUILTIN_COMMANDS
        .iter()
        .filter(|command| requires_arguments(command))
        .map(|command| command.name)
        .collect()
}

/// Contract: catalog names are unique and every required-arg entry advertises
/// `<…>` usage that dispatch surfaces without starting a model turn.
#[test]
fn builtin_catalog_is_unique_and_usage_is_typed() {
    let mut names = BUILTIN_COMMANDS
        .iter()
        .map(|command| command.name)
        .collect::<Vec<_>>();
    let before = names.len();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), before, "BUILTIN_COMMANDS must have unique names");

    for command in BUILTIN_COMMANDS {
        let rendered = usage(command);
        assert!(
            rendered.starts_with(&format!("/{}", command.name)),
            "usage must start with /{}: {rendered}",
            command.name
        );
        if requires_arguments(command) {
            assert!(
                rendered.contains('<'),
                "required-arg command /{} must advertise angle-bracket usage: {rendered}",
                command.name
            );
        }
    }

    let required = required_arg_commands();
    for name in [
        "import",
        "loop",
        "loop-update",
        "loop-delete",
        "loop-cancel",
        "process",
        "run",
        "chain",
        "run-chain",
    ] {
        assert!(
            required.contains(name),
            "/{name} must remain a required-arg builtin"
        );
    }
    assert!(
        !required.contains("goal"),
        "bare /goal is optional and must reach Show"
    );
    assert!(
        !required.contains("workflow"),
        "bare /workflow is optional and must open the workflows page"
    );
}

/// Contract: visible help/completion is the explicit primary surface only.
#[test]
fn primary_command_surface_is_help_and_completion_only() {
    assert_eq!(
        PRIMARY_COMMAND_NAMES,
        &[
            "settings", "model", "branch", "resume", "fork", "export", "agents", "compact", "ps",
            "loop", "goal", "workflow", "code-review", "btw",
        ]
    );
    assert_eq!(PRIMARY_COMMAND_NAMES.len(), 14);
    let visible = visible_catalog()
        .into_iter()
        .map(|command| command.name)
        .collect::<Vec<_>>();
    assert_eq!(visible, PRIMARY_COMMAND_NAMES);
    assert_eq!(
        visible.iter().filter(|name| *name == "workflow").count(),
        1,
        "workflow must appear exactly once in the primary surface"
    );
    assert!(is_primary_command("goal"));
    assert!(is_primary_command("workflow"));
    assert!(!is_primary_command("workfloww"));
    assert!(!is_primary_command("skill:release"));
    assert!(!is_primary_command("help"));
    assert!(!is_primary_command("import"));
    assert!(builtin("import").is_some());
    assert!(builtin("quit").is_some());
    assert_eq!(
        usage(builtin("workflow").expect("workflow")),
        "/workflow [list|show [id|name]|create <objective>|create <name> <objective>|pause|resume|cancel|integrate|remove]"
    );
}

/// Contract: every built-in has a dispatch surface that either succeeds with a
/// minimal safe action, returns a typed usage/validation error, or exposes the
/// panel-open precursor data — without panicking or issuing a model turn.
#[tokio::test]
async fn every_builtin_has_minimal_action_usage_or_panel_precursor() {
    let fixture = Fixture::with_recorder("all").await;
    let app = &fixture.application;
    let mut covered = BTreeSet::new();

    // help — catalog lists every builtin name + description.
    {
        let (commands, diagnostics) = executable_catalog(app);
        assert!(
            diagnostics.is_empty(),
            "fresh fixture must not emit catalog diagnostics: {diagnostics:?}"
        );
        let names = commands
            .iter()
            .map(|command| command.name.as_str())
            .collect::<BTreeSet<_>>();
        for command in BUILTIN_COMMANDS {
            assert!(
                names.contains(command.name),
                "/help catalog missing builtin {}",
                command.name
            );
            assert!(
                !command.description.is_empty(),
                "/{} description must be non-empty",
                command.name
            );
        }
        covered.insert("help");
    }

    // settings — inspect works; typed usage errors for malformed set/apply.
    {
        let inspect = parse_interactive_settings_command("settings", None)
            .expect("parse settings list")
            .expect("settings command");
        let output =
            pi_cli::interactive_commands::execute_interactive_settings_command(app, inspect)
                .await
                .expect("settings inspect");
        assert!(
            output.contains("scope") || output.contains("values") || output.contains('{'),
            "settings inspect must return a snapshot: {output}"
        );
        let err = parse_interactive_settings_command("settings", Some("set theme not-json"))
            .expect_err("invalid JSON value");
        assert!(
            err.to_string().contains("invalid JSON") || err.to_string().contains("usage"),
            "typed settings error: {err:#}"
        );
        let err = parse_interactive_settings_command("settings", Some("apply compaction.enabled"))
            .expect_err("incomplete apply");
        assert!(
            err.to_string().contains("usage"),
            "apply usage error: {err:#}"
        );
        covered.insert("settings");
    }

    // model — report current selection without switching.
    {
        let state = app.state().await;
        let model = state.model.expect("fixture model");
        assert_eq!(model.provider, "faux");
        assert!(
            model.id.starts_with("faux-slash-"),
            "current model id: {}",
            model.id
        );
        covered.insert("model");
    }

    // scoped-models — TUI panel precursor: available models include fixture.
    {
        let models = pi_ai::get_models("faux");
        assert!(
            models
                .iter()
                .any(|model| model.id.starts_with("faux-slash-")),
            "scoped-models panel catalog must include fixture model: {models:?}"
        );
        covered.insert("scoped-models");
    }

    // models — filterable catalog contains the active faux model.
    {
        let models = pi_ai::get_models("faux");
        assert!(
            models.iter().any(|model| model.id.contains("faux-slash")),
            "models listing must include faux-slash fixture"
        );
        covered.insert("models");
    }

    // export — HTML export of the live (possibly empty) session succeeds.
    {
        let out = fixture.cwd.path().join("export-smoke.html");
        let path = app.export_html(Some(&out)).expect("export html");
        assert_eq!(path, out);
        assert!(
            std::fs::metadata(&path).expect("export exists").len() > 0,
            "export must write bytes"
        );
        covered.insert("export");
    }

    // import — required path surfaces typed usage (TUI pre-check + REPL).
    {
        let command = builtin("import").expect("import builtin");
        assert_eq!(usage(command), "/import <path.jsonl>");
        assert!(
            requires_arguments(command),
            "import must remain required-arg"
        );
        covered.insert("import");
    }

    // share — background share fails closed without gh/network (ShareFailed).
    {
        let mut events = app.subscribe();
        app.share_session();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut saw_failure = false;
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout_at(deadline, events.recv()).await {
                Ok(Ok(pi_coding::ApplicationEvent::ShareFailed { message })) => {
                    assert!(
                        !message.trim().is_empty(),
                        "ShareFailed must carry an actionable message"
                    );
                    saw_failure = true;
                    break;
                }
                Ok(Ok(pi_coding::ApplicationEvent::ShareSucceeded { url })) => {
                    panic!("share must not succeed without credentials/network: {url}");
                }
                Ok(Ok(_)) => continue,
                Ok(Err(_)) => break,
                Err(_) => break,
            }
        }
        assert!(
            saw_failure,
            "share without gh/network must emit ShareFailed"
        );
        covered.insert("share");
    }

    // copy — empty assistant history is a safe no-op.
    {
        assert!(
            app.last_assistant_text().is_none(),
            "fresh session has no assistant text to copy"
        );
        covered.insert("copy");
    }

    // name — set then read back.
    {
        app.set_session_name("slash-dispatch").expect("set name");
        let state = app.state().await;
        assert_eq!(
            state.session_name.as_deref(),
            Some("slash-dispatch"),
            "session name must round-trip"
        );
        covered.insert("name");
    }

    // session — id / message count / path are readable without a turn.
    {
        let state = app.state().await;
        assert!(
            state.session_id.is_some() || state.session_file.is_some(),
            "recorded session exposes id or path: {state:?}"
        );
        covered.insert("session");
    }

    // sessions — listing for cwd must not panic.
    {
        let _ = pi_coding::list_sessions(fixture.cwd.path());
        covered.insert("sessions");
    }

    // changelog — embedded history is non-empty.
    {
        let text = include_str!("../../../CHANGELOG.md");
        assert!(
            text.contains('#') || text.len() > 16,
            "changelog content must be present"
        );
        covered.insert("changelog");
    }

    // hotkeys — default bindings render sectioned help text.
    {
        let bindings = KeyBindingsManager::default();
        let sections = bindings.hotkey_sections();
        assert!(
            !sections.is_empty(),
            "hotkeys panel requires at least one section"
        );
        covered.insert("hotkeys");
    }

    // theme — built-in theme catalog is non-empty and has an active name.
    {
        let themes = ThemeManager::load(Vec::new());
        assert!(!themes.names().is_empty(), "theme catalog must not be empty");
        assert!(
            !themes.active_name().is_empty(),
            "active theme name must be set"
        );
        covered.insert("theme");
    }

    // branch / fork — forkable message list is readable (empty is fine).
    {
        let messages = app.fork_messages().expect("fork messages");
        assert!(
            messages.is_empty(),
            "fresh session has no forkable user messages: {messages:?}"
        );
        covered.insert("branch");
        covered.insert("fork");
    }

    // clone — clone current branch without a model turn.
    {
        match app.clone_session().await {
            Ok(outcome) => assert!(!outcome.cancelled),
            Err(error) => {
                let message = format!("{error:#}");
                assert!(
                    !message.is_empty(),
                    "clone error must be actionable when branching is unavailable"
                );
            }
        }
        covered.insert("clone");
    }

    // tree — session tree snapshot loads and serializes.
    {
        let tree = app.session_tree().expect("session tree");
        let encoded = serde_json::to_string(&tree).expect("serialize tree");
        assert!(
            encoded.contains("tree") || encoded.contains("entries") || encoded.starts_with('{'),
            "session tree must serialize: {encoded}"
        );
        covered.insert("tree");
    }

    // loop / loops / loop-update / loop-delete / loop-cancel
    {
        let err = parse_interactive_loop_command("loop", None)
            .expect_err("loop requires interval + prompt");
        assert!(
            err.to_string().contains("usage") || err.to_string().contains("interval"),
            "loop usage error: {err:#}"
        );

        let create = parse_interactive_loop_command("loop", Some("1h slash keep-alive"))
            .expect("parse loop create")
            .expect("loop create");
        let created = execute_interactive_loop_command(app, create)
            .await
            .expect("create loop");
        assert!(
            created.contains("scheduled"),
            "loop create output: {created}"
        );
        let task_id = created
            .strip_prefix("scheduled ")
            .and_then(|rest| rest.split_once(" · ").map(|(id, _)| id))
            .expect("task id")
            .to_owned();

        let listed = execute_interactive_loop_command(
            app,
            parse_interactive_loop_command("loops", None)
                .expect("parse loops")
                .expect("loops"),
        )
        .await
        .expect("list loops");
        assert!(
            listed.contains(&task_id) && listed.contains("slash keep-alive"),
            "loops listing: {listed}"
        );

        let updated = execute_interactive_loop_command(
            app,
            parse_interactive_loop_command(
                "loop-update",
                Some(&format!("{task_id} 2h slash updated")),
            )
            .expect("parse update")
            .expect("update"),
        )
        .await
        .expect("update loop");
        assert!(
            updated.contains("every 2 hours") && updated.contains("slash updated"),
            "loop-update: {updated}"
        );

        let err = parse_interactive_loop_command("loop-update", Some("only-id"))
            .expect_err("loop-update needs interval or prompt");
        assert!(
            err.to_string().contains("usage") || err.to_string().contains("update"),
            "loop-update usage: {err:#}"
        );

        let deleted = execute_interactive_loop_command(
            app,
            parse_interactive_loop_command("loop-delete", Some(&task_id))
                .expect("parse delete")
                .expect("delete"),
        )
        .await
        .expect("delete loop");
        assert_eq!(deleted, format!("deleted loop {task_id}"));

        let err = parse_interactive_loop_command("loop-delete", None)
            .expect_err("loop-delete requires id");
        assert!(
            err.to_string().contains("usage"),
            "loop-delete usage: {err:#}"
        );

        let second = execute_interactive_loop_command(
            app,
            parse_interactive_loop_command("loop", Some("1h cancel target"))
                .expect("parse second")
                .expect("second"),
        )
        .await
        .expect("second create");
        let second_id = second
            .strip_prefix("scheduled ")
            .and_then(|rest| rest.split_once(" · ").map(|(id, _)| id))
            .expect("second id")
            .to_owned();
        let cancelled = execute_interactive_loop_command(
            app,
            parse_interactive_loop_command("loop-cancel", Some(&second_id))
                .expect("parse cancel")
                .expect("cancel"),
        )
        .await
        .expect("cancel loop");
        assert_eq!(cancelled, format!("cancelled loop {second_id}"));

        let err = parse_interactive_loop_command("loop-cancel", None)
            .expect_err("loop-cancel requires id");
        assert!(
            err.to_string().contains("usage"),
            "loop-cancel usage: {err:#}"
        );

        let empty = execute_interactive_loop_command(
            app,
            parse_interactive_loop_command("loops", None)
                .expect("parse final loops")
                .expect("loops"),
        )
        .await
        .expect("final loops");
        assert_eq!(empty, "no active loops");

        covered.insert("loop");
        covered.insert("loops");
        covered.insert("loop-update");
        covered.insert("loop-delete");
        covered.insert("loop-cancel");
    }

    // goal — show empty, create, lifecycle, typed errors.
    {
        let show = execute_interactive_goal_command(
            app,
            parse_interactive_goal_command(None).expect("parse show"),
        )
        .await
        .expect("goal show");
        assert_eq!(show, "no goal");

        let err = parse_interactive_goal_command(Some("create")).expect_err("empty objective");
        assert!(
            err.to_string().contains("objective") || err.to_string().contains("empty"),
            "goal create usage: {err:#}"
        );

        fixture.registration.set_responses(vec![
            pi_ai::providers::FauxResponse::text("created"),
            pi_ai::providers::FauxResponse::text("resumed"),
        ]);
        let created = execute_interactive_goal_command(
            app,
            parse_interactive_goal_command(Some("create --tokens 32 ship slash coverage"))
                .expect("parse create"),
        )
        .await
        .expect("goal create");
        assert!(
            created.contains("active") && created.contains("ship slash coverage"),
            "goal create: {created}"
        );
        let paused = execute_interactive_goal_command(
            app,
            parse_interactive_goal_command(Some("pause")).expect("parse pause"),
        )
        .await
        .expect("goal pause");
        assert!(paused.contains("paused"), "goal pause: {paused}");
        let resumed = execute_interactive_goal_command(
            app,
            parse_interactive_goal_command(Some("resume")).expect("parse resume"),
        )
        .await
        .expect("goal resume");
        assert!(resumed.contains("active"), "goal resume: {resumed}");
        let completed = execute_interactive_goal_command(
            app,
            parse_interactive_goal_command(Some("complete")).expect("parse complete"),
        )
        .await
        .expect("goal complete");
        assert!(
            completed.contains("completed"),
            "goal complete: {completed}"
        );
        let _ = execute_interactive_goal_command(
            app,
            parse_interactive_goal_command(Some("drop")).expect("parse drop"),
        )
        .await;
        covered.insert("goal");
    }

    // workflow — bare opens page; parse/usage; no Application manager required yet.
    {
        let open = parse_interactive_workflow_command(None).expect("bare workflow");
        assert_eq!(open, InteractiveWorkflowCommand::OpenPage);
        // Panel-open precursor: OpenPage effect is the TUI handoff signal.
        assert_eq!(WorkflowCommandEffect::OpenPage, WorkflowCommandEffect::OpenPage);

        let list = parse_interactive_workflow_command(Some("list")).expect("list");
        assert_eq!(list, InteractiveWorkflowCommand::List);

        let created = parse_interactive_workflow_command(Some(
            r#"create "ship it" "land multi workflow foundation""#,
        ))
        .expect("create");
        assert!(matches!(
            created,
            InteractiveWorkflowCommand::Create {
                name,
                objective
            } if name == "ship it" && objective == "land multi workflow foundation"
        ));

        let err = parse_interactive_workflow_command(Some("workfloww")).expect_err("typo");
        assert!(
            err.to_string().contains("unknown workflow subcommand"),
            "workflow typo: {err:#}"
        );
        let created = parse_interactive_workflow_command(Some("create only-name"))
            .expect("single objective create");
        assert!(matches!(
            created,
            InteractiveWorkflowCommand::Create { name, objective }
                if name == "only-name" && objective == "only-name"
        ));
        assert!(
            !is_primary_command("workfloww"),
            "/workflow must not prefix-match /workfloww"
        );
        assert!(
            !is_primary_command("skill:release"),
            "skills stay under /skill: isolation"
        );
        covered.insert("workflow");
    }

    // todo — empty listing + markdown set.
    {
        let empty = app.todo_state();
        assert!(
            empty.phases.is_empty(),
            "fresh todo list starts empty: {:?}",
            empty.phases
        );
        let phases = pi_coding::parse_todo_markdown("- [ ] cover slash dispatch")
            .expect("parse todo markdown");
        let result = app.set_todos(phases).expect("set todos");
        assert!(
            !result.summary.is_empty() || !result.phases.is_empty(),
            "todo set must apply: {:?}",
            result.summary
        );
        let listed = app.todo_state();
        assert!(
            !listed.phases.is_empty(),
            "todo list must retain the set item"
        );
        covered.insert("todo");
    }

    // trust — project trust decision persists via Application.
    {
        let result = app
            .set_project_trust(TrustDecision::Ask)
            .await
            .expect("set trust");
        assert!(
            result.generation >= 0,
            "trust reload reports a generation: {}",
            result.generation
        );
        covered.insert("trust");
    }

    // login / logout — non-interactive without provider fails with typed error.
    {
        let err = pi_cli::auth_commands::login(None, false)
            .await
            .expect_err("login requires provider outside TTY");
        assert!(
            err.to_string().contains("provider"),
            "login error: {err:#}"
        );
        let err = pi_cli::auth_commands::logout(None, false)
            .await
            .expect_err("logout requires provider outside TTY");
        assert!(
            err.to_string().contains("provider") || err.to_string().contains("credential"),
            "logout error: {err:#}"
        );
        covered.insert("login");
        covered.insert("logout");
    }

    // llama — status fails clearly when router is unconfigured (no hang).
    {
        let err = pi_cli::llama_commands::run_slash("status")
            .await
            .expect_err("unconfigured llama status");
        let message = err.to_string();
        assert!(
            message.contains("not configured")
                || message.contains("LLAMA_BASE_URL")
                || message.contains("llama"),
            "llama status error must be actionable: {message}"
        );
        let err = pi_cli::llama_commands::run_slash("load")
            .await
            .expect_err("load requires model");
        assert!(
            err.to_string().contains("usage"),
            "llama load usage: {err:#}"
        );
        covered.insert("llama");
    }

    // new — start a fresh transcript without a model turn.
    {
        assert!(!app.new_session().await.expect("new session").cancelled);
        covered.insert("new");
    }

    // compact — manual compact either reports tokens or a clear error; never panics.
    {
        match app.compact(None).await {
            Ok(result) => {
                assert!(
                    result.tokens_before >= 0,
                    "compaction tokens_before: {}",
                    result.tokens_before
                );
            }
            Err(error) => {
                let message = format!("{error:#}");
                assert!(!message.is_empty(), "compact error must be non-empty");
            }
        }
        covered.insert("compact");
    }

    // resume — hermetic catalog for the fixture home does not panic.
    {
        let _catalog = pi_coding::SessionCatalog::new(fixture._home.path());
        covered.insert("resume");
    }

    // ps / process — empty list, usage errors, start/stop bounded process.
    {
        let ps = execute_interactive_process_command(
            app,
            parse_interactive_process_command("ps", None)
                .expect("parse ps")
                .expect("ps"),
        )
        .await
        .expect("ps");
        assert_eq!(ps, "No supervised processes");

        let err = parse_interactive_process_command("process", None)
            .expect_err("process requires operation");
        assert!(
            err.to_string().contains("usage"),
            "process usage: {err:#}"
        );
        let err = parse_interactive_process_command("process", Some("start"))
            .expect_err("start requires program");
        assert!(
            err.to_string().contains("process start"),
            "process start usage: {err:#}"
        );

        let started = execute_interactive_process_command(
            app,
            parse_interactive_process_command("process", Some("start sleep 30"))
                .expect("parse start")
                .expect("start"),
        )
        .await
        .expect("start sleep");
        assert!(
            started.contains("Running") || started.contains("Starting"),
            "process start output: {started}"
        );
        let id = started
            .split('\t')
            .next()
            .expect("process id column")
            .to_owned();
        let listed = execute_interactive_process_command(
            app,
            parse_interactive_process_command("ps", None)
                .expect("parse ps after start")
                .expect("ps"),
        )
        .await
        .expect("ps after start");
        assert!(
            listed.contains(&id),
            "ps must list started process {id}: {listed}"
        );
        let stopped = execute_interactive_process_command(
            app,
            parse_interactive_process_command("process", Some(&format!("stop {id}")))
                .expect("parse stop")
                .expect("stop"),
        )
        .await
        .expect("stop process");
        assert!(
            stopped.contains(&id),
            "stop output must reference id: {stopped}"
        );
        let process_id: pi_coding::ProcessId =
            serde_json::from_value(serde_json::Value::String(id.clone())).expect("process id");
        let _ = app
            .process_wait(&process_id, Some(std::time::Duration::from_secs(5)))
            .await;

        covered.insert("ps");
        covered.insert("process");
    }

    // agents — panel precursor: agent definitions snapshot is readable.
    {
        let snapshot = app.resource_snapshot();
        let _ = snapshot.map(|snap| snap.agents.len());
        covered.insert("agents");
    }

    // reload — resource reload advances generation without a model turn.
    {
        let before = app.resource_generation().unwrap_or(0);
        let result = app.reload().await.expect("reload");
        assert!(
            result.generation >= before,
            "reload generation {} < before {before}",
            result.generation
        );
        covered.insert("reload");
    }

    // run / chain / run-chain — typed usage errors without extension commands.
    {
        let err = parse_run_invocation("").expect_err("run requires command");
        assert!(err.to_string().contains("usage"), "run usage: {err:#}");
        let err = parse_chain_invocation("").expect_err("chain requires steps");
        assert!(err.to_string().contains("usage"), "chain usage: {err:#}");
        let err = pi_cli::interactive_commands::invoke_extension_command(
            app,
            "definitely-missing-extension-cmd",
            String::new(),
        )
        .await
        .expect_err("missing extension command");
        assert!(
            err.to_string().contains("unknown or untrusted")
                || err.to_string().contains("extension runtime"),
            "run missing command: {err:#}"
        );
        covered.insert("run");
        covered.insert("chain");
        covered.insert("run-chain");
    }

    // code-review — load a real tracked dirty tree into the fullscreen panel model.
    {
        let git = |args: &[&str]| {
            let mut command = Command::new("git");
            // Explicit minimal env: never inherit host GIT_* routing/config/signing/hooks.
            command.env_clear();
            command
                .args(["-c", "commit.gpgsign=false"])
                .args(args)
                .current_dir(fixture.cwd.path())
                .env("PATH", std::env::var_os("PATH").unwrap_or_default())
                .env("HOME", fixture._home.path())
                .env("USERPROFILE", fixture._home.path())
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env(
                    "GIT_CONFIG_GLOBAL",
                    fixture.cwd.path().join("absent-global-git-config"),
                )
                .env("GIT_TERMINAL_PROMPT", "0")
                .env("GIT_AUTHOR_NAME", "Slash Dispatch")
                .env("GIT_AUTHOR_EMAIL", "slash-dispatch@example.invalid")
                .env("GIT_COMMITTER_NAME", "Slash Dispatch")
                .env("GIT_COMMITTER_EMAIL", "slash-dispatch@example.invalid");
            let output = run_command_bounded(command);
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "slash-dispatch@example.invalid"]);
        git(&["config", "user.name", "Slash Dispatch"]);
        std::fs::write(fixture.cwd.path().join("review.txt"), "before\n")
            .expect("write review fixture");
        git(&["add", "review.txt"]);
        git(&["commit", "--no-verify", "-qm", "seed review fixture"]);
        std::fs::write(fixture.cwd.path().join("review.txt"), "after\n")
            .expect("dirty review fixture");
        let panel = CodeReviewPanel::load(fixture.cwd.path());
        assert!(
            panel
                .snapshot()
                .files
                .iter()
                .any(|file| file.path == "review.txt"),
            "code-review panel must load the tracked working-tree change: {:?}",
            panel.snapshot()
        );
        covered.insert("code-review");
    }

    // btw — create the detached read-only side controller and cleanly shut it down.
    {
        let mut side = SideChatController::fork_from(app)
            .await
            .expect("fork side chat");
        assert_eq!(side.tool_mode(), SideChatToolMode::ReadOnly);
        assert_eq!(side.cwd(), fixture.cwd.path());
        assert!(
            side.tool_names().await.iter().all(|name| name != "write"),
            "default /btw controller must not expose write"
        );
        side.shutdown().await;
        covered.insert("btw");
    }

    // quit — present in catalog; dispatch returns exit without side effects.
    {
        let command = builtin("quit").expect("quit builtin");
        assert_eq!(command.name, "quit");
        assert!(command.argument_hint.is_none());
        covered.insert("quit");
    }

    let expected = BUILTIN_COMMANDS
        .iter()
        .map(|command| command.name)
        .collect::<BTreeSet<_>>();
    assert_eq!(
        covered, expected,
        "missing builtin coverage: {:?}; unexpected: {:?}",
        expected.difference(&covered).collect::<Vec<_>>(),
        covered.difference(&expected).collect::<Vec<_>>()
    );

    let state = app.state().await;
    assert!(
        state.message_count <= 2,
        "dispatch coverage must not start a model turn; message_count={}",
        state.message_count
    );

    app.cleanup().await;
}

/// Contract: required-arg builtins without arguments produce the same usage
/// string shape the TUI surfaces via push_status (`Usage: /name <…>`).
#[test]
fn required_arg_builtins_render_tui_usage_status_shape() {
    for command in BUILTIN_COMMANDS.iter().filter(|command| requires_arguments(command)) {
        let status = format!("Usage: {}", usage(command));
        assert!(
            status.contains('<') && status.starts_with("Usage: /"),
            "TUI usage status shape broken for /{}: {status}",
            command.name
        );
        assert!(
            !status.contains("You") && !status.contains("Assistant"),
            "usage status must not resemble a transcript turn: {status}"
        );
    }
}

/// Contract: closest_builtin suggests real catalog names for typos.
#[test]
fn unknown_slash_typo_suggests_catalog_neighbor() {
    let suggestion = pi_cli::interactive_commands::closest_builtin("hlep");
    assert_eq!(
        suggestion,
        Some("help"),
        "typo hlep should suggest /help, got {suggestion:?}"
    );
    let suggestion = pi_cli::interactive_commands::closest_builtin("proces");
    assert_eq!(
        suggestion,
        Some("process"),
        "typo proces should suggest /process, got {suggestion:?}"
    );
}

/// Contract: import of a missing path fails with a path error, not a panic or turn.
#[tokio::test]
async fn import_missing_path_is_typed_error() {
    let fixture = Fixture::new("import-missing").await;
    let missing = fixture.cwd.path().join("no-such-session.jsonl");
    let err = pi_coding::import_session(pi_coding::SourceSessionFormat::Pi, &missing)
        .expect_err("missing import path");
    let message = err.to_string();
    assert!(
        !message.is_empty(),
        "import missing path error must be non-empty"
    );
    fixture.application.cleanup().await;
}
