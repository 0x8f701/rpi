//! Persistent persona CLI-surface tests: the `/persona` slash-command parse +
//! execute path (`pi_cli::interactive_commands`) against a real `Application`
//! whose attached `ResourceManager` discovers a durable persona from a temp
//! agent dir.
//!
//! Contracts defended (deterministic, isolated temp agent dir, no model calls):
//! - **Discovery (CLI view)**: `/persona` lists the discovered persona.
//! - **Show / Current / Clear**: render details, the no-selection state, and
//!   clear the persisted preferred-agent setting.
//! - **Reset containment**: `/persona reset <name> --yes` clears `memory/` and
//!   `sessions/` under the persona root but keeps `persona.md`.
//! - **Remove containment**: `/persona remove <name> --yes` deletes `persona.md`
//!   but keeps `memory/` and `sessions/`, then reloads so the persona leaves
//!   the catalog.
//! - **Purge containment**: `/persona remove <name> --purge --yes` deletes the
//!   whole persona root, then reloads so the persona leaves the catalog.
//! - **--yes confirmation**: destructive ops without `--yes` are rejected by the
//!   parser before any filesystem mutation.
//! - **Unknown persona**: destructive ops on an unknown name fail closed.

use pi_ai::providers::{FauxProviderOptions, FauxResponse, register_faux_provider};
use pi_ai::{Model, StopReason};
use pi_cli::interactive_commands::{
    execute_interactive_persona_command, parse_interactive_persona_command,
};
use pi_coding::{
    Application, ResourceManager, ResourceManagerOptions, Session, SessionOptions,
};
/// Per-process isolated native session root so `start_new_recording()` never
/// writes into the real `~/.pi/agent/sessions` tree (Web sidebar source).
fn test_sessions_root() -> std::path::PathBuf {
    static ROOT: std::sync::LazyLock<tempfile::TempDir> = std::sync::LazyLock::new(|| {
        tempfile::tempdir().expect("test sessions root")
    });
    ROOT.path().to_path_buf()
}

fn persona_dir(agent_dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    let dir = agent_dir.join("personas").join(name);
    std::fs::create_dir_all(&dir).expect("persona root dir");
    std::fs::write(
        dir.join("persona.md"),
        format!("---\nname: {name}\ndescription: durable {name}\n---\n{name} prompt"),
    )
    .expect("persona.md");
    dir
}

/// Seeds a persona root with a memory entry and a session archive so reset vs
/// remove containment is observable.
fn seed_persona_state(persona_root: &std::path::Path) {
    let memory = persona_root.join("memory");
    std::fs::create_dir_all(&memory).expect("memory dir");
    std::fs::write(
        memory.join("entries.jsonl"),
        "{\"id\":\"a\",\"content\":\"persona-memory-note\",\"tags\":[],\"ts\":1,\"session\":\"s\"}\n",
    )
    .expect("entries");
    let sessions = persona_root.join("sessions");
    std::fs::create_dir_all(&sessions).expect("sessions dir");
    std::fs::write(sessions.join("Mentor.jsonl"), "{}\n").expect("archive");
}

fn faux_session() -> (Session, pi_ai::providers::FauxProviderRegistration) {
    let suffix = uuid::Uuid::now_v7().simple().to_string();
    let api = format!("persona-cli-api-{suffix}");
    let provider = format!("persona-cli-provider-{suffix}");
    let model = Model {
        id: format!("persona-cli-model-{suffix}"),
        name: "Persona CLI Model".to_owned(),
        api: api.clone(),
        provider: provider.clone(),
        ..Model::default()
    };
    let registration = register_faux_provider(FauxProviderOptions {
        api,
        provider,
        models: vec![model.clone()],
        chunk_size: 1,
    });
    registration.set_responses(vec![FauxResponse {
        content: vec![pi_ai::ContentBlock::text("done")],
        stop_reason: StopReason::Stop,
        error_message: None,
    }]);
    let session = Session::new(SessionOptions {
        model,
        cwd: std::env::current_dir().expect("cwd"),
        system_prompt: String::new(),
        thinking_level: pi_agent::ThinkingLevel::Off,
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
    session.set_session_dir(test_sessions_root());
    session.start_new_recording().expect("start recording");
    (session, registration)
}

struct PersonaApp {
    application: Application,
    _registration: pi_ai::providers::FauxProviderRegistration,
    persona_root: std::path::PathBuf,
    _root: tempfile::TempDir,
}

/// Builds an Application whose attached ResourceManager discovers a seeded
/// durable persona from a temp agent dir. No orchestration runtime is enabled,
/// which is sufficient for list/show/current/clear and the destructive ops.
async fn persona_app(seed_state: bool) -> PersonaApp {
    let root = tempfile::tempdir().expect("root");
    let agent_dir = root.path().join("agent");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    let persona_root = persona_dir(&agent_dir, "mentor");
    if seed_state {
        seed_persona_state(&persona_root);
    }
    let cwd = root.path().join("project");
    std::fs::create_dir_all(&cwd).expect("cwd");
    let mut options = ResourceManagerOptions::new(&cwd);
    options.agent_dir = agent_dir;
    options.disable_extensions = true;
    options.disable_skills = true;
    options.disable_prompt_templates = true;
    options.disable_themes = true;
    options.disable_context_files = true;
    let resources = ResourceManager::new(options).expect("resources");
    let (session, registration) = faux_session();
    session
        .attach_resources(resources)
        .await
        .expect("attach resources");
    let application = Application::new(session).await;
    PersonaApp {
        application,
        _registration: registration,
        persona_root,
        _root: root,
    }
}

#[tokio::test]
async fn persona_cli_lists_shows_current_and_clears() {
    let app = persona_app(false).await;
    let application = &app.application;

    let list = execute_interactive_persona_command(
        application,
        parse_interactive_persona_command(None).expect("list"),
    )
    .await
    .expect("list");
    assert!(list.contains("mentor"), "list must name the persona: {list}");

    let show = execute_interactive_persona_command(
        application,
        parse_interactive_persona_command(Some("mentor")).expect("show"),
    )
    .await
    .expect("show");
    assert!(show.contains("mentor"), "show must name the persona: {show}");
    assert!(
        show.to_lowercase().contains("persona"),
        "show must identify it as a persona: {show}"
    );

    let current = execute_interactive_persona_command(
        application,
        parse_interactive_persona_command(Some("--current")).expect("current"),
    )
    .await
    .expect("current");
    assert!(
        current.contains("No persona selected"),
        "no preferred persona yet: {current}"
    );

    let cleared = execute_interactive_persona_command(
        application,
        parse_interactive_persona_command(Some("--clear")).expect("clear"),
    )
    .await
    .expect("clear");
    assert!(
        cleared.contains("cleared"),
        "clear must report the preference cleared: {cleared}"
    );

    app.application.cleanup().await;
}

#[tokio::test]
async fn persona_cli_reset_clears_state_keeps_definition() {
    let app = persona_app(true).await;
    let persona_root = app.persona_root.clone();
    assert!(persona_root.join("memory").exists(), "memory seeded");
    assert!(persona_root.join("sessions").exists(), "sessions seeded");

    let command = parse_interactive_persona_command(Some("reset mentor --yes"))
        .expect("reset parse");
    let outcome = execute_interactive_persona_command(&app.application, command)
        .await
        .expect("reset execute");
    assert!(
        outcome.contains("reset"),
        "reset must report the outcome: {outcome}"
    );

    assert!(
        persona_root.join("persona.md").exists(),
        "reset must keep persona.md"
    );
    assert!(
        !persona_root.join("memory").exists(),
        "reset must clear the persona memory directory"
    );
    assert!(
        !persona_root.join("sessions").exists(),
        "reset must clear the persona sessions directory"
    );
    app.application.cleanup().await;
}

#[tokio::test]
async fn persona_cli_remove_deletes_definition_keeps_state() {
    let app = persona_app(true).await;
    let persona_root = app.persona_root.clone();

    let command = parse_interactive_persona_command(Some("remove mentor --yes"))
        .expect("remove parse");
    let outcome = execute_interactive_persona_command(&app.application, command)
        .await
        .expect("remove execute");
    assert!(
        outcome.contains("removed"),
        "remove must report the outcome: {outcome}"
    );

    assert!(
        !persona_root.join("persona.md").exists(),
        "remove must delete persona.md"
    );
    assert!(
        persona_root.join("memory").exists(),
        "remove must keep the persona memory directory"
    );
    assert!(
        persona_root.join("sessions").exists(),
        "remove must keep the persona sessions directory"
    );

    // After reload the persona leaves the catalog.
    let list = execute_interactive_persona_command(
        &app.application,
        parse_interactive_persona_command(None).expect("list"),
    )
    .await
    .expect("list after remove");
    assert!(
        !list.contains("mentor"),
        "removed persona must not appear in the catalog: {list}"
    );
    app.application.cleanup().await;
}

#[tokio::test]
async fn persona_cli_purge_deletes_root() {
    let app = persona_app(true).await;
    let persona_root = app.persona_root.clone();

    let command = parse_interactive_persona_command(Some("remove mentor --purge --yes"))
        .expect("purge parse");
    let outcome = execute_interactive_persona_command(&app.application, command)
        .await
        .expect("purge execute");
    assert!(
        outcome.contains("purged"),
        "purge must report the outcome: {outcome}"
    );

    assert!(
        !persona_root.exists(),
        "purge must delete the entire persona root"
    );

    let list = execute_interactive_persona_command(
        &app.application,
        parse_interactive_persona_command(None).expect("list"),
    )
    .await
    .expect("list after purge");
    assert!(
        !list.contains("mentor"),
        "purged persona must not appear in the catalog: {list}"
    );
    app.application.cleanup().await;
}

#[tokio::test]
async fn persona_cli_destructive_requires_yes_and_rejects_unknown() {
    let app = persona_app(true).await;
    let persona_root = app.persona_root.clone();

    // --yes is required for every destructive op; the parser rejects before
    // any filesystem mutation.
    for arg in ["reset mentor", "remove mentor", "remove mentor --purge"] {
        let error = parse_interactive_persona_command(Some(arg))
            .expect_err("destructive op without --yes must fail to parse");
        assert!(
            error.to_string().contains("--yes"),
            "expected --yes requirement for {arg:?}: {error}"
        );
    }
    assert!(
        persona_root.join("persona.md").exists(),
        "no mutation without --yes confirmation"
    );

    // An unknown persona name fails closed.
    let command = parse_interactive_persona_command(Some("reset nope --yes"))
        .expect("parse unknown reset");
    let error = execute_interactive_persona_command(&app.application, command)
        .await
        .expect_err("unknown persona must fail");
    assert!(
        error.to_string().contains("unknown persona"),
        "expected unknown-persona error: {error}"
    );
    app.application.cleanup().await;
}